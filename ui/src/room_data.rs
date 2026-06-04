#![allow(dead_code)]

use crate::util::ecies::{
    decrypt_secret_from_member_blob_raw, encrypt_secret_for_member, seal_bytes,
};
use crate::util::get_current_system_time;
use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
use river_core::chat_delegate::RoomKey;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::AuthorizedMember;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::MessageId;
use river_core::room_state::privacy::{
    PrivacyMode, RoomCipherSpec, RoomDisplayMetadata, SealedBytes,
};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, EncryptedSecretForMemberV1,
    SecretVersionRecordV1,
};
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, PartialEq)]
pub enum SendMessageError {
    UserNotMember,
    UserBanned,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomData {
    pub owner_vk: VerifyingKey,
    pub room_state: ChatRoomStateV1,
    pub self_sk: SigningKey,
    pub contract_key: ContractKey,
    /// The last message ID that was read by the user (for unread tracking).
    /// Messages after this ID from other users are considered unread when
    /// computing the title badge.
    ///
    /// Advanced when (a) the user opens this room, (b) a message arrives
    /// while the user is viewing this room with the tab visible, and
    /// (c) the tab transitions from visible to hidden — at which point every
    /// room is advanced to its latest message, so only messages arriving
    /// *after* the tab is hidden contribute to the badge.
    ///
    /// Persisted to delegate storage.
    #[serde(default)]
    pub last_read_message_id: Option<MessageId>,
    /// All decrypted room secrets by version (if room is private)
    /// Maps secret_version -> decrypted 32-byte secret
    #[serde(skip)]
    pub secrets: HashMap<u32, [u8; 32]>,
    /// The current (latest) secret version
    #[serde(skip)]
    pub current_secret_version: Option<u32>,
    /// When the secret was last rotated (for weekly rotation checks)
    #[serde(skip)]
    pub last_secret_rotation: Option<std::time::SystemTime>,
    /// Whether the signing key has been migrated to the delegate
    /// This is runtime state and not persisted - checked on each startup
    #[serde(skip)]
    pub key_migrated_to_delegate: bool,
    /// The user's own AuthorizedMember, stored so they can re-add themselves
    /// after being pruned for inactivity (no recent messages).
    #[serde(default)]
    pub self_authorized_member: Option<AuthorizedMember>,
    /// The invite chain members needed to validate self_authorized_member.
    /// Contains all members in the chain from self up to (but not including) the owner.
    #[serde(default)]
    pub invite_chain: Vec<AuthorizedMember>,
    /// The user's own AuthorizedMemberInfo, stored so their nickname survives
    /// being pruned for inactivity and re-added.
    #[serde(default)]
    pub self_member_info: Option<AuthorizedMemberInfo>,
    /// The plaintext nickname the local user has chosen for this room.
    ///
    /// Set when the invitation is accepted and kept in step with later
    /// nickname edits, so it always reflects the user's current choice.
    /// The member-info rebuild paths ([`RoomData::build_member_info_heal`]
    /// and [`RoomData::build_rejoin_delta`]) consult it to restore the
    /// user's *chosen* nickname rather than a generated default handle.
    ///
    /// It is needed because `self_member_info` cannot always be built at
    /// join time: a private room whose secret has not yet arrived can't
    /// seal the nickname, so the member_info is deferred to the self-heal
    /// — and by then the join-time `PendingRoomJoin` (the only other place
    /// the choice was recorded) has been discarded.
    ///
    /// Stored in plaintext even for a private room. That is not a new
    /// exposure: the persisted `RoomData` already carries `self_sk`, from
    /// which every room secret — and hence every sealed nickname — is
    /// derivable, so the room secret, not the nickname, is the thing that
    /// must be protected. The rebuild paths still seal it before it is
    /// published into a private room. `None` for the owner, for rooms
    /// joined before this field existed, and for imported rooms.
    #[serde(default)]
    pub self_nickname: Option<String>,
    /// The previous contract key before WASM update, used for migration fallback.
    /// When the bundled WASM changes, this stores the old contract key so
    /// any client can GET state from the old contract and PUT it to the new one.
    #[serde(default)]
    pub previous_contract_key: Option<ContractKey>,
    /// Room secrets recovered from the invitation artifact, by version.
    ///
    /// Populated once, when an invitation to a private room is accepted,
    /// from the `room_secrets` the inviting member embedded in the
    /// `Invitation`. Persisted (rides inside the `rooms_data` delegate
    /// blob) so an invitee who has not yet received the owner delegate's
    /// `encrypted_secrets` back-fill can still rebuild the
    /// `#[serde(skip)]` `secrets` map after a page refresh.
    ///
    /// Plaintext, like `self_sk` and `self_nickname` already in this
    /// struct — not a new exposure class, since the persisted `RoomData`
    /// already carries `self_sk`. Folded into `secrets` by
    /// [`RoomData::repopulate_secrets_from_state`]. Empty for public
    /// rooms, for the room owner, and for rooms joined before this field
    /// existed.
    #[serde(default)]
    pub invitation_secrets: HashMap<u32, [u8; 32]>,
}

/// Compute the `SealedBytes` for an invitee's chosen nickname at join time.
///
/// For a private room, prefer the secret carried in the freshly-fetched
/// network `state` ([`current_secret_from_state`]); fall back to a secret
/// supplied out-of-band by the invitation artifact, so a brand-new invitee
/// can seal their nickname WITHOUT waiting for the owner delegate's
/// `encrypted_secrets` back-fill. Returns `None` for a private room when no
/// secret is available from either source — the caller then defers
/// `member_info` to the self-heal path rather than leaking a plaintext
/// nickname into a private room. A public room always returns `Some`
/// (plaintext seal).
pub(crate) fn seal_invitee_nickname(
    state: &ChatRoomStateV1,
    self_sk: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    preferred_nickname: &str,
) -> Option<SealedBytes> {
    if state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        return Some(SealedBytes::public(preferred_nickname.as_bytes().to_vec()));
    }
    // Fallback: the invitation-carried secret for the room's CURRENT
    // version — the nickname must be sealed at `current_version`. An
    // invitation created before a rotation has no entry at the new
    // `current_version`, so this correctly yields `None` and the caller
    // defers `member_info` to the self-heal path.
    let (secret, version) = current_secret_from_state(state, self_sk).or_else(|| {
        let version = state.secrets.current_version;
        invitation_secrets
            .get(&version)
            .map(|secret| (*secret, version))
    })?;
    Some(seal_bytes(preferred_nickname.as_bytes(), &secret, version))
}

/// Decrypt the room's current-version secret out of a raw network
/// `ChatRoomStateV1`, for the member who holds `self_sk`.
///
/// Mirrors the per-blob decrypt loop in
/// [`RoomData::repopulate_secrets_from_state`], but for the single
/// current version and reading straight from the supplied `state` —
/// callers (the invitation-accept PUT path, `build_member_info_heal`)
/// need the secret derived from the freshly-fetched NETWORK state, not
/// from a possibly-stale `RoomData`. Returns `None` for a public room
/// (no secret), when the blob for the current version has not been
/// issued for this member yet, or when decryption fails.
pub(crate) fn current_secret_from_state(
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

impl RoomData {
    /// Regenerate the contract_key from the owner_vk using the current WASM.
    /// This ensures the contract_key always matches the bundled WASM, which may
    /// have been updated since the room was first created/stored.
    /// Saves the old contract key to `previous_contract_key` if it changed.
    pub fn regenerate_contract_key(&mut self) {
        let params = ChatRoomParametersV1 {
            owner: self.owner_vk,
        };
        let params_bytes = to_cbor_vec(&params);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let new_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);
        if new_key != self.contract_key {
            self.previous_contract_key = Some(self.contract_key);
        }
        self.contract_key = new_key;
    }

    /// Get the room key for delegate operations (owner's verifying key bytes)
    pub fn room_key(&self) -> RoomKey {
        self.owner_vk.to_bytes()
    }

    /// Check if the room state has been populated from the network.
    /// A room that was just imported (or created but not yet synced) will have
    /// a default configuration signed by a zero key, not the real owner.
    /// This is used to show a "Syncing..." indicator and disable message input
    /// until the real room state arrives from the network.
    ///
    /// Checks that the configuration signature verifies against the owner's key.
    /// The default AuthorizedConfigurationV1 is signed by SigningKey([0; 32]),
    /// which will fail verification against any real owner key. This works for
    /// both owner and non-owner imports.
    pub fn is_awaiting_initial_sync(&self) -> bool {
        self.room_state
            .configuration
            .verify_signature(&self.owner_vk)
            .is_err()
    }

    /// Check if the room is in private mode
    pub fn is_private(&self) -> bool {
        matches!(
            self.room_state.configuration.configuration.privacy_mode,
            river_core::room_state::privacy::PrivacyMode::Private
        )
    }

    /// Get the current (latest) secret for encryption/decryption
    pub fn get_secret(&self) -> Option<(&[u8; 32], u32)> {
        self.current_secret_version
            .and_then(|v| self.secrets.get(&v).map(|s| (s, v)))
    }

    /// Get a secret for a specific version (for decrypting old content)
    pub fn get_secret_for_version(&self, version: u32) -> Option<&[u8; 32]> {
        self.secrets.get(&version)
    }

    /// Rebuild `recent_messages.actions_state` (edits, deletes, reactions)
    /// from this room's action messages, decrypting private action payloads
    /// with the in-memory secrets.
    ///
    /// `ComposableState::apply_delta` for `MessagesV1` ends every merge with
    /// the *non-decrypting* `rebuild_actions_state()`, which can only decode
    /// PUBLIC action messages. For a private room that call clears
    /// `actions_state` and re-derives it from public actions only — so every
    /// edit / delete / reaction carried by a PRIVATE action message is wiped
    /// until a later decrypt-aware rebuild restores it.
    ///
    /// The network ingestion paths (`apply_delta_inner`,
    /// `update_room_state_inner`, the GET handler) already follow their
    /// `apply_delta`/`merge` with a decrypt-aware rebuild. The local
    /// optimistic send/edit/delete/react handlers in `conversation.rs` did
    /// not, which is why an edited private-room message briefly reverted to
    /// its original text whenever a new message (or any other local action)
    /// was sent — the optimistic `apply_delta` ran the public-only rebuild
    /// and dropped the edit until the network echo re-applied it
    /// (freenet/river#310).
    ///
    /// Call this after any local `apply_delta` that mutates a private room's
    /// `recent_messages`. No-op on public rooms (the public rebuild that
    /// `apply_delta` already ran is correct and complete).
    pub fn rebuild_private_actions_state(&mut self) {
        use crate::util::ecies::decrypt_with_symmetric_key;
        use river_core::room_state::message::RoomMessageBody;

        if !self.is_private() {
            return;
        }

        // Decrypt all private action messages using version-aware lookup.
        let decrypted_actions: HashMap<MessageId, Vec<u8>> = self
            .room_state
            .recent_messages
            .messages
            .iter()
            .filter(|msg| msg.message.content.is_action())
            .filter_map(|msg| {
                if let RoomMessageBody::Private {
                    ciphertext,
                    nonce,
                    secret_version,
                    ..
                } = &msg.message.content
                {
                    self.get_secret_for_version(*secret_version)
                        .and_then(|secret| {
                            decrypt_with_symmetric_key(secret, ciphertext, nonce)
                                .ok()
                                .map(|plaintext| (msg.id(), plaintext))
                        })
                } else {
                    None
                }
            })
            .collect();

        self.room_state
            .recent_messages
            .rebuild_actions_state_with_decrypted(&decrypted_actions);
    }

    /// Get a reference to the current secret (convenience method)
    pub fn current_secret(&self) -> Option<&[u8; 32]> {
        self.current_secret_version
            .and_then(|v| self.secrets.get(&v))
    }

    /// Set/add a room secret for a specific version
    pub fn set_secret(&mut self, secret: [u8; 32], version: u32) {
        self.secrets.insert(version, secret);
        // Update current version if this is a newer version
        if self.current_secret_version.is_none_or(|v| version >= v) {
            self.current_secret_version = Some(version);
            self.last_secret_rotation = Some(get_current_system_time());
        }
    }

    /// Decrypt any `EncryptedSecretForMemberV1` blobs in the merged room
    /// state into the in-memory [`Self::secrets`] map for every version
    /// not already present, and align [`Self::current_secret_version`]
    /// with the contract's `current_version`.
    ///
    /// No-op on public rooms.
    ///
    /// Must be called on EVERY private-room state ingestion path —
    /// initial GET, full-state update, delta apply, delegate-load merge —
    /// because `secrets` is `#[serde(skip)]` (rebuilt from encrypted
    /// blobs each time) AND because the chat delegate's PR #245
    /// back-fill of `encrypted_secrets` for a newly-joined member is
    /// asynchronous from the initial subscribe. Before #251 only the
    /// initial-load paths ran this loop, so the post-subscribe update
    /// carrying the back-filled blob never repopulated the map and the
    /// new member rendered every message as
    /// `[Encrypted message - secret vN not available]` until they hard-
    /// refreshed.
    ///
    /// Also folds in [`Self::invitation_secrets`] — secrets carried in the
    /// invitation artifact — for any version the contract has not provided
    /// an owner-signed blob for. The owner-signed contract blob is
    /// authoritative and overwrites an invitation-carried value at the same
    /// version (and prunes it from `invitation_secrets`).
    ///
    /// Returns the number of new versions decrypted (for logging).
    pub fn repopulate_secrets_from_state(&mut self) -> usize {
        use dioxus::logger::tracing::warn;

        if !self.is_private() {
            return 0;
        }

        // (secret_version, ciphertext, nonce, sender_ephemeral_x25519_pk_bytes)
        type PendingBlob = (u32, Vec<u8>, [u8; 12], [u8; 32]);

        let member_id = MemberId::from(&self.self_sk.verifying_key());

        // Snapshot the member's encrypted_secrets blobs so we can release
        // the borrow on `room_state` before the `&mut self` calls below.
        //
        // We deliberately do NOT filter out versions already in `secrets`:
        // the owner-signed contract blob is authoritative and MUST be able
        // to overwrite an (unauthenticated) value a prior `invitation_secrets`
        // fold placed at the same version — otherwise a malicious or buggy
        // inviter who supplied a wrong secret would permanently shadow the
        // authentic blob for the rest of the session. Re-decrypting the
        // handful of own-member blobs on each ingestion is negligible.
        let pending: Vec<PendingBlob> = self
            .room_state
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.member_id == member_id)
            .map(|s| {
                (
                    s.secret.secret_version,
                    s.secret.ciphertext.clone(),
                    s.secret.nonce,
                    s.secret.sender_ephemeral_public_key,
                )
            })
            .collect();

        let self_sk = self.self_sk.clone();
        let mut decrypted_count = 0usize;
        for (version, ciphertext, nonce, ephemeral_key_bytes) in pending {
            match decrypt_secret_from_member_blob_raw(
                &ciphertext,
                &nonce,
                &ephemeral_key_bytes,
                &self_sk,
            ) {
                Ok(secret) => {
                    let is_new = !self.secrets.contains_key(&version);
                    // `set_secret` inserts/overwrites — the owner-signed
                    // contract secret is authoritative.
                    self.set_secret(secret, version);
                    // It supersedes any invitation-carried copy at this
                    // version: drop it so a stale/garbage invitation secret
                    // cannot resurface and is not re-persisted in
                    // `rooms_data`.
                    self.invitation_secrets.remove(&version);
                    if is_new {
                        decrypted_count += 1;
                    }
                }
                Err(e) => {
                    warn!(
                        "repopulate_secrets_from_state: failed to decrypt v{} for member {:?}: {}",
                        version, member_id, e
                    );
                }
            }
        }

        // Fold in any secrets recovered from the invitation artifact
        // (`Invitation::room_secrets`, copied into `invitation_secrets` at
        // accept time) for versions the contract has NOT yet provided an
        // owner-signed blob for. This lets an invitee read a private room
        // before the owner delegate's `encrypted_secrets` back-fill
        // arrives, and survive a refresh (`secrets` is `#[serde(skip)]`
        // while `invitation_secrets` is persisted). The contract loop above
        // runs first and removes from `invitation_secrets` every version it
        // covers, so the owner-signed value always wins. Cloned to release
        // the `&self` borrow before the `&mut self` `set_secret` calls.
        let invitation_secrets = self.invitation_secrets.clone();
        for (version, secret) in invitation_secrets {
            if !self.secrets.contains_key(&version) {
                self.set_secret(secret, version);
                decrypted_count += 1;
            }
        }

        // Align `current_secret_version` with the contract's notion of
        // current, preserving the existing get_response / load-rooms
        // behaviour. `set_secret` only ever advances the pointer; this
        // explicit assignment also covers the case where the blob for
        // `current_version` hasn't arrived yet (we'll fall back to
        // `None` in `get_secret()` until it does, which makes the send
        // path no-op rather than encrypt with a stale key).
        //
        // The assignment is unconditional and CAN move the pointer
        // backwards in the pathological case where local state holds a
        // newer decrypted version than the post-merge contract state.
        // This relies on the `RoomSecretsV1` invariant
        // (`common/src/room_state/secret.rs:166-174,192-213`) that
        // `current_version == max(versions)` and is monotonically
        // non-decreasing under merge — so the merge that immediately
        // precedes this call cannot move `current_version` backwards.
        let current_version = self.room_state.secrets.current_version;
        self.current_secret_version = Some(current_version);

        decrypted_count
    }

    /// Check if the secret needs rotation (weekly rotation or never rotated)
    /// Only applies to private rooms owned by this user.
    ///
    /// As of #228 PR 2 v2 the weekly rotation trigger has been removed (it
    /// only fired while the UI was open, which defeated the point of a
    /// scheduled rotation). The remaining UI-side rotation triggers — owner
    /// banning a member, owner clicking Rotate manually — call
    /// [`RoomData::rotate_secret`] directly. The chat delegate also drives
    /// rotation asynchronously via ContractNotification when the UI isn't
    /// active. Both produce byte-identical secrets via
    /// [`river_core::key_derivation::derive_room_secret`], so concurrent
    /// rotations converge via the contract's CRDT (duplicate-version dedup
    /// at `secret.rs:140-145`).
    ///
    /// This helper is retained for any future caller that wants to ask
    /// "is this room overdue for rotation?", but no UI sync trigger calls
    /// it any more.
    pub fn needs_secret_rotation(&self) -> bool {
        // Only check for private rooms
        if !self.is_private() {
            return false;
        }

        // Only the owner can rotate
        if self.owner_vk != self.self_sk.verifying_key() {
            return false;
        }

        // Check if we have a last rotation time
        match self.last_secret_rotation {
            None => {
                // Never rotated, check if room has been around for a week
                // Get the creation time from the first secret version
                if let Some(first_version) = self.room_state.secrets.versions.first() {
                    let creation_time = first_version.record.created_at;
                    if let Ok(duration) = get_current_system_time().duration_since(creation_time) {
                        // Rotate if it's been more than 7 days since creation
                        return duration.as_secs() > 7 * 24 * 60 * 60;
                    }
                }
                false
            }
            Some(last_rotation) => {
                // Check if it's been more than 7 days since last rotation
                if let Ok(duration) = get_current_system_time().duration_since(last_rotation) {
                    duration.as_secs() > 7 * 24 * 60 * 60
                } else {
                    false
                }
            }
        }
    }

    /// Check if the user can send a message in the room.
    /// A user is considered a member if they are the owner, are in the active
    /// members list, or have a stored invitation (self_authorized_member).
    pub fn can_send_message(&self) -> Result<(), SendMessageError> {
        let verifying_key = self.self_sk.verifying_key();
        let member_id = MemberId::from(&verifying_key);

        // Check if banned first
        if self
            .room_state
            .bans
            .0
            .iter()
            .any(|b| b.ban.banned_user == member_id)
        {
            return Err(SendMessageError::UserBanned);
        }

        // Owner can always send
        if verifying_key == self.owner_vk {
            return Ok(());
        }

        // Currently in members list
        if self
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == verifying_key)
        {
            return Ok(());
        }

        // Has stored invite (can re-add with first message)
        if self.self_authorized_member.is_some() {
            return Ok(());
        }

        Err(SendMessageError::UserNotMember)
    }

    /// Check if the user can participate in the room (send messages, edit profile).
    /// Returns Ok if user is not banned AND (is owner OR has self_authorized_member OR is in members list).
    pub fn can_participate(&self) -> Result<(), SendMessageError> {
        let verifying_key = self.self_sk.verifying_key();
        let member_id = MemberId::from(&verifying_key);

        // Check if banned first
        if self
            .room_state
            .bans
            .0
            .iter()
            .any(|b| b.ban.banned_user == member_id)
        {
            return Err(SendMessageError::UserBanned);
        }

        // Owner can always participate
        if verifying_key == self.owner_vk {
            return Ok(());
        }

        // Currently in members list
        if self
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == verifying_key)
        {
            return Ok(());
        }

        // Has stored invite (was previously a member, can re-add)
        if self.self_authorized_member.is_some() {
            return Ok(());
        }

        Err(SendMessageError::UserNotMember)
    }

    /// Capture the user's AuthorizedMember and MemberInfo from the current state.
    /// AuthorizedMember is only captured once (migration path for older rooms).
    /// MemberInfo is always updated to the latest version so nickname edits are preserved.
    pub fn capture_self_membership_data(&mut self, parameters: &ChatRoomParametersV1) {
        let verifying_key = self.self_sk.verifying_key();
        if verifying_key == self.owner_vk {
            return; // Owner doesn't need this
        }

        // Always update self_member_info to latest version. `self_nickname`
        // is deliberately NOT refreshed here: it is the lower-priority
        // fallback (the member-info rebuild paths prefer `self_member_info`),
        // and it is kept current at its own write sites — invitation accept
        // and nickname edit. A stale `self_nickname` can never override a
        // newer `self_member_info`.
        let member_id = MemberId::from(&verifying_key);
        if let Some(info) = self
            .room_state
            .member_info
            .member_info
            .iter()
            .filter(|i| i.member_info.member_id == member_id)
            .max_by_key(|i| i.member_info.version)
        {
            self.self_member_info = Some(info.clone());
        }

        // Only capture authorized member once
        if self.self_authorized_member.is_some() {
            return;
        }
        if let Some(member) = self
            .room_state
            .members
            .members
            .iter()
            .find(|m| m.member.member_vk == verifying_key)
        {
            self.self_authorized_member = Some(member.clone());
            // Capture invite chain
            if let Ok(chain) = self.room_state.members.get_invite_chain(member, parameters) {
                self.invite_chain = chain;
            }
        }
    }

    /// Record the membership credentials from an accepted invitation.
    ///
    /// Sets, as a set, the three `self_*` fields the rejoin and member-info
    /// self-heal paths depend on: the user's `AuthorizedMember`, their
    /// `AuthorizedMemberInfo` (`None` when it could not be built at join
    /// time — a private room whose secret had not yet arrived to seal the
    /// nickname), and the plaintext nickname they chose. Kept as one method
    /// so a caller cannot set `self_member_info` for an accepted invite and
    /// forget `self_nickname` — the omission that silently dropped the
    /// user's nickname before. See [`RoomData::build_member_info_heal`] and
    /// [`RoomData::build_rejoin_delta`].
    pub fn record_invite_credentials(
        &mut self,
        authorized_member: AuthorizedMember,
        member_info: Option<AuthorizedMemberInfo>,
        nickname: String,
    ) {
        self.self_authorized_member = Some(authorized_member);
        self.self_member_info = member_info;
        self.self_nickname = Some(nickname);
    }

    /// Keep the cached `self_*` fields in step after the local user edits a
    /// nickname — but only when the edited member *is* the local user.
    ///
    /// Both `self_member_info` and `self_nickname` feed the member-info
    /// rebuild paths ([`RoomData::build_member_info_heal`] and
    /// [`RoomData::build_rejoin_delta`]), which prefer `self_member_info`.
    /// Updating both here means a strand or inactivity-rejoin that happens
    /// between the edit and the next sync round-trip republishes the
    /// *edited* nickname, not the pre-edit one. A no-op when `edited_member`
    /// is someone else (their member_info is not ours to cache).
    pub fn record_self_nickname_edit(
        &mut self,
        edited_member: MemberId,
        new_member_info: AuthorizedMemberInfo,
        nickname: String,
    ) {
        if MemberId::from(&self.self_sk.verifying_key()) != edited_member {
            return;
        }
        self.self_member_info = Some(new_member_info);
        self.self_nickname = Some(nickname);
    }

    /// Build the members + member_info deltas needed to re-add ourselves to
    /// the room after being pruned for inactivity.
    ///
    /// Returns `(None, None)` if we're already a member or don't have stored
    /// credentials to re-add. The member_info element is `None` while the
    /// members element is still `Some` for a private room whose secret is
    /// not available to seal the nickname — we re-add the member but leave
    /// the member_info to the GET-path self-heal rather than leak a
    /// plaintext nickname.
    pub fn build_rejoin_delta(
        &self,
    ) -> (
        Option<river_core::room_state::member::MembersDelta>,
        Option<Vec<AuthorizedMemberInfo>>,
    ) {
        let self_vk = self.self_sk.verifying_key();

        // Owner is never pruned
        let is_in_members = self_vk == self.owner_vk
            || self
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == self_vk);

        if is_in_members {
            return (None, None);
        }

        let Some(ref authorized_member) = self.self_authorized_member else {
            return (None, None);
        };

        let current_member_ids: std::collections::HashSet<_> = self
            .room_state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();

        let mut members_to_add = vec![authorized_member.clone()];
        for chain_member in &self.invite_chain {
            if !current_member_ids.contains(&chain_member.member.id()) {
                members_to_add.push(chain_member.clone());
            }
        }

        // Reuse the already-published `self_member_info` when we have one —
        // it carries the user's chosen nickname, correctly versioned and
        // sealed. For a private room only reuse a `Private`-sealed entry: a
        // stale `Public`-sealed entry (e.g. one left behind by a
        // public→private reconfiguration) must not be republished as
        // plaintext — the same guard `build_member_info_heal` applies.
        // Otherwise rebuild the entry from `self_nickname` (the nickname
        // the user chose), falling back to a generated default handle only
        // when nothing is on record — the same nickname resolution
        // priority `build_member_info_heal` uses.
        let reusable_stored = self.self_member_info.as_ref().filter(|stored| {
            !self.is_private()
                || matches!(
                    stored.member_info.preferred_nickname,
                    SealedBytes::Private { .. }
                )
        });
        let authorized_info: Option<AuthorizedMemberInfo> =
            if let Some(stored_info) = reusable_stored {
                Some(stored_info.clone())
            } else {
                let member_id = MemberId::from(&self_vk);
                let existing_version = self
                    .room_state
                    .member_info
                    .member_info
                    .iter()
                    .find(|i| i.member_info.member_id == member_id)
                    .map(|i| i.member_info.version)
                    .unwrap_or(0);
                let nickname = self
                    .self_nickname
                    .clone()
                    .unwrap_or_else(|| crate::nickname::generate_default_nickname(&self_vk));
                // A private room's nickname must be encrypted. Seal it with
                // the current room secret; if no secret is available publish
                // NO member_info (the members delta still re-adds us) rather
                // than leak a plaintext nickname — the GET-path self-heal
                // restores it later.
                let sealed = if self.is_private() {
                    self.get_secret()
                        .map(|(secret, version)| seal_bytes(nickname.as_bytes(), secret, version))
                } else {
                    Some(SealedBytes::public(nickname.into_bytes()))
                };
                sealed.map(|preferred_nickname| {
                    AuthorizedMemberInfo::new_with_member_key(
                        MemberInfo {
                            member_id,
                            version: existing_version,
                            preferred_nickname,
                        },
                        &self.self_sk,
                    )
                })
            };

        (
            Some(river_core::room_state::member::MembersDelta::new(
                members_to_add,
            )),
            authorized_info.map(|info| vec![info]),
        )
    }

    /// Self-heal for the PR #272 "Unknown member" regression.
    ///
    /// If the network's canonical `state` shows this user in `members`
    /// but with no matching `member_info` entry, they render as
    /// "Unknown" to every other peer. Returns a self-signed
    /// `AuthorizedMemberInfo` to re-publish so the entry is restored;
    /// returns `None` when there is nothing to heal — the user is the
    /// owner, is not a member of `state`, or already has a `member_info`
    /// entry.
    ///
    /// The room contract only accepts a non-owner's `member_info` when
    /// it is self-signed by that member's own key, so a stranded member
    /// can only be healed by their own client. That is exactly what this
    /// produces — using `self_sk`. It cannot be done owner-side or for
    /// any other member.
    ///
    /// Privacy mode and the room secret are read from the supplied
    /// network `state`, never from `self` — for an imported room `self`'s
    /// `room_state`/`secrets` are a stale public placeholder at the time
    /// this runs, and trusting them would mis-seal a private nickname.
    ///
    /// The nickname is resolved in priority order: the stored
    /// `self_member_info` (a nickname the user has already published),
    /// then `self_nickname` (the nickname they chose at join time, kept
    /// for exactly this case — see that field's docs), then a generated
    /// default handle as a last resort. This is what stops the heal from
    /// silently replacing a user-chosen nickname with a generated one
    /// when `self_member_info` could not be built at join time.
    ///
    /// For a **public** room the chosen entry/nickname is used directly.
    ///
    /// For a **private** room the nickname must be encrypted: a stored
    /// `self_member_info` is reused only if already `Private`-sealed,
    /// otherwise the resolved nickname is freshly `Private`-sealed. If the
    /// room secret is not yet present in `state` this returns `None`
    /// (deferring the heal) rather than publish a plaintext nickname.
    pub fn build_member_info_heal(&self, state: &ChatRoomStateV1) -> Option<AuthorizedMemberInfo> {
        let self_vk = self.self_sk.verifying_key();
        if self_vk == self.owner_vk {
            return None; // the owner's member_info is managed separately
        }
        let member_id = MemberId::from(&self_vk);

        let in_members = state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == self_vk);
        if !in_members {
            return None; // not a member on the network — nothing to heal
        }
        let has_member_info = state
            .member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == member_id);
        if has_member_info {
            return None; // already present — not stranded
        }

        // Stranded — re-publish our own member_info.
        //
        // Privacy mode and the room secret are read from the freshly-
        // fetched network `state`, NOT from `self.room_state` /
        // `self.secrets` / `self.get_secret()`. For an imported room
        // those reflect a stale public placeholder and an empty secret
        // map at heal-build time (the merge runs later, in a deferred
        // closure), so trusting `self` would misclassify a private room
        // as public and seal the nickname in plaintext.
        let is_private = state.configuration.configuration.privacy_mode == PrivacyMode::Private;

        // A PRIVATE room's nickname must be encrypted. A stored entry is
        // reusable only if it is already Private-sealed; otherwise mint a
        // fresh Private-sealed default handle, and if the room secret is
        // not yet present in `state` defer the heal entirely (return
        // `None`) rather than leak a plaintext nickname — the member
        // stays "Unknown" until a later GET once the secret has arrived.
        if is_private {
            if let Some(stored) = &self.self_member_info {
                if matches!(
                    stored.member_info.preferred_nickname,
                    SealedBytes::Private { .. }
                ) {
                    return Some(stored.clone());
                }
            }
            let (secret, version) = current_secret_from_state(state, &self.self_sk)?;
            // The nickname the user picked at join time if we still have
            // it (the common case for a private-room join whose seal was
            // deferred — see `self_nickname`), else a generated default.
            let nickname = self
                .self_nickname
                .clone()
                .unwrap_or_else(|| crate::nickname::generate_default_nickname(&self_vk));
            // version: 0 is safe — the heal only fires when no member_info
            // entry exists in `state` (the `has_member_info` check
            // above), so this is never version-compared against an
            // existing entry.
            let info = MemberInfo {
                member_id,
                version: 0,
                preferred_nickname: seal_bytes(nickname.as_bytes(), &secret, version),
            };
            return Some(AuthorizedMemberInfo::new_with_member_key(
                info,
                &self.self_sk,
            ));
        }

        // Public room — a public nickname is not sensitive. Prefer an
        // already-known entry so the user keeps their chosen nickname.
        if let Some(stored) = &self.self_member_info {
            return Some(stored.clone());
        }

        // No published member_info — use the nickname the user picked at
        // join time if we still have it, else a deterministic default.
        // version: 0 is safe for the reason noted in the private branch.
        let nickname = self
            .self_nickname
            .clone()
            .unwrap_or_else(|| crate::nickname::generate_default_nickname(&self_vk));
        let info = MemberInfo {
            member_id,
            version: 0,
            preferred_nickname: SealedBytes::public(nickname.into_bytes()),
        };
        Some(AuthorizedMemberInfo::new_with_member_key(
            info,
            &self.self_sk,
        ))
    }

    pub fn owner_id(&self) -> MemberId {
        self.owner_vk.into()
    }

    /// Replace an existing member entry with a new authorized member
    /// Returns true if the member was found and updated
    pub fn restore_member_access(
        &mut self,
        old_member_vk: VerifyingKey,
        new_authorized_member: AuthorizedMember,
    ) -> bool {
        // Find and replace the member entry
        if let Some(member) = self
            .room_state
            .members
            .members
            .iter_mut()
            .find(|m| m.member.member_vk == old_member_vk)
        {
            *member = new_authorized_member;
            true
        } else {
            false
        }
    }

    pub fn parameters(&self) -> ChatRoomParametersV1 {
        ChatRoomParametersV1 {
            owner: self.owner_vk,
        }
    }

    /// Rotate the room secret, generating a new secret and encrypting it for
    /// all current members. Banned members are excluded. Returns a
    /// `SecretsDelta` with the new secret version and encrypted secrets.
    ///
    /// **Synchronous fast-path (UI-driven, #228 PR 2 v2):** this is the
    /// hot path the UI takes when the owner is actively driving a state
    /// change — banning a member, clicking Manual Rotate. Doing the
    /// rotation synchronously matters because both cases need the next
    /// owner-sent message to be encrypted under a key the just-banned
    /// member cannot decrypt; routing rotation through a delegate
    /// ContractNotification round-trip would leak that one message.
    ///
    /// The chat delegate also rotates via ContractNotification when the
    /// UI isn't actively driving (auto-prune from message lifecycle, peer
    /// state updates received in the background, etc.). Both paths
    /// produce **byte-identical** secrets because they both call
    /// [`river_core::key_derivation::derive_room_secret`] with the same
    /// `(signing_key_seed, owner_vk, new_version)` triple. Concurrent
    /// rotation by both paths therefore converges via the contract's
    /// duplicate-version dedup in `apply_delta` (`secret.rs:140-145`):
    /// whichever record lands first wins, the other is rejected as a
    /// duplicate, and both replicas end up with the same authoritative
    /// state.
    pub fn rotate_secret(
        &mut self,
    ) -> Result<river_core::room_state::secret::SecretsDelta, String> {
        use river_core::room_state::secret::SecretsDelta;

        // Only allow rotation for private rooms
        if !self.is_private() {
            return Err("Cannot rotate secret for public room".to_string());
        }

        // Only the room owner can rotate secrets
        if self.owner_vk != self.self_sk.verifying_key() {
            return Err("Only room owner can rotate secrets".to_string());
        }

        // Get current version and increment. Bail on overflow so we don't
        // wrap to 0 and collide with the existing version-0 record.
        let current_version = self.room_state.secrets.current_version;
        if current_version == u32::MAX {
            return Err(format!(
                "Refusing to rotate: current secret version is u32::MAX ({}). \
                 This is effectively unreachable in practice but the overflow \
                 case must not silently wrap to 0.",
                current_version
            ));
        }
        let new_version = current_version + 1;

        // Derive the new secret deterministically from the signing-key seed,
        // owner VK, and target version. Two devices owned by the same person
        // therefore produce byte-identical secrets without coordination, and
        // the delegate's parallel rotation pipeline (also using
        // `derive_room_secret`) converges with this UI path via the
        // contract's CRDT dedup.
        let new_secret = river_core::key_derivation::derive_room_secret(
            &self.self_sk.to_bytes(),
            &self.owner_vk,
            new_version,
        );

        // Create the secret version record
        let secret_version = SecretVersionRecordV1 {
            version: new_version,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: get_current_system_time(),
        };

        let authorized_version = AuthorizedSecretVersionRecord::new(secret_version, &self.self_sk);

        // Get all current members, excluding banned members. We pair
        // each `MemberId` with their `VerifyingKey` so the shared
        // back-fill helper can encrypt for them directly.
        let banned_members: std::collections::HashSet<MemberId> = self
            .room_state
            .bans
            .0
            .iter()
            .map(|b| b.ban.banned_user)
            .collect();

        let owner_id = MemberId::from(&self.owner_vk);
        let current_members_with_vks: Vec<(MemberId, ed25519_dalek::VerifyingKey)> = self
            .room_state
            .members
            .members
            .iter()
            .map(|m| (MemberId::from(&m.member.member_vk), m.member.member_vk))
            .filter(|(id, _)| !banned_members.contains(id) && *id != owner_id)
            .collect();

        if current_members_with_vks.is_empty() {
            return Err("No members to encrypt secret for".to_string());
        }

        use dioxus::logger::tracing::info;
        info!(
            "Rotating secret to version {} for {} members",
            new_version,
            current_members_with_vks.len()
        );

        // Delegate to the shared back-fill helper so the UI synchronous
        // fast-path emits BYTE-IDENTICAL blob sets to the delegate's
        // asynchronous catch-up path. Critically, this also back-fills
        // prior versions for any current member who lacks a blob at
        // that version — without this, a newly-joined invitee who
        // arrives between rotations would never receive secrets for
        // anything but `new_version`, leaving them unable to decrypt
        // the room name / pre-join messages. See Bug #3 PR B
        // (Ivvor 2026-05-17).
        let new_encrypted_secrets =
            river_core::room_state::secret::build_rotation_encrypted_secrets(
                &self.self_sk,
                &self.owner_vk,
                owner_id,
                new_version,
                &new_secret,
                &current_members_with_vks,
                &self.room_state.secrets.encrypted_secrets,
            )?;

        // Update our local secrets (add new version, keep old ones for decryption)
        self.secrets.insert(new_version, new_secret);
        self.current_secret_version = Some(new_version);
        self.last_secret_rotation = Some(get_current_system_time());

        Ok(SecretsDelta {
            current_version: Some(new_version),
            new_versions: vec![authorized_version],
            new_encrypted_secrets,
        })
    }

    /// Generate encrypted secrets for members who don't have them yet
    /// Returns a SecretsDelta if secrets were generated, None otherwise
    pub fn generate_missing_member_secrets(
        &self,
    ) -> Option<river_core::room_state::secret::SecretsDelta> {
        use river_core::room_state::secret::SecretsDelta;

        // Only generate secrets if this is a private room and we have the secret
        if !self.is_private() {
            return None;
        }

        let (room_secret, current_version) = self.get_secret()?;

        // Get all current members
        let member_ids: Vec<MemberId> = self
            .room_state
            .members
            .members
            .iter()
            .map(|m| MemberId::from(&m.member.member_vk))
            .collect();

        // Find members who don't have encrypted secrets for the current version
        let members_with_secrets: std::collections::HashSet<MemberId> = self
            .room_state
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.secret_version == current_version)
            .map(|s| s.secret.member_id)
            .collect();

        let members_without_secrets: Vec<_> = member_ids
            .into_iter()
            .filter(|id| !members_with_secrets.contains(id))
            .collect();

        if members_without_secrets.is_empty() {
            return None;
        }

        use dioxus::logger::tracing::info;
        info!(
            "Generating encrypted secrets for {} members",
            members_without_secrets.len()
        );

        // Generate encrypted secrets for each member
        let mut new_encrypted_secrets = Vec::new();

        for member_id in members_without_secrets {
            // Find the member's verifying key
            if let Some(member) = self
                .room_state
                .members
                .members
                .iter()
                .find(|m| MemberId::from(&m.member.member_vk) == member_id)
            {
                let member_vk = member.member.member_vk;

                // Encrypt the room secret for this member
                let (ciphertext, nonce, ephemeral_key) =
                    encrypt_secret_for_member(room_secret, &member_vk);

                // Create the encrypted secret record
                let encrypted_secret = EncryptedSecretForMemberV1 {
                    member_id,
                    secret_version: current_version,
                    ciphertext,
                    nonce,
                    sender_ephemeral_public_key: ephemeral_key.to_bytes(),
                    provider: self.owner_vk.into(),
                };

                let authorized_encrypted_secret =
                    AuthorizedEncryptedSecretForMember::new(encrypted_secret, &self.self_sk);

                new_encrypted_secrets.push(authorized_encrypted_secret);
            }
        }

        if new_encrypted_secrets.is_empty() {
            return None;
        }

        Some(SecretsDelta {
            current_version: None,
            new_versions: vec![],
            new_encrypted_secrets,
        })
    }
}

pub struct CurrentRoom {
    pub owner_key: Option<VerifyingKey>,
}

impl CurrentRoom {
    pub fn owner_id(&self) -> Option<MemberId> {
        self.owner_key.map(|vk| vk.into())
    }

    pub fn owner_key(&self) -> Option<&VerifyingKey> {
        self.owner_key.as_ref()
    }
}

impl PartialEq for CurrentRoom {
    fn eq(&self, other: &Self) -> bool {
        self.owner_key == other.owner_key
    }
}

/// Per-room notification preference (local user setting). Controls when a
/// browser notification fires for new messages in a room.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum NotificationMode {
    /// Notify for every message from another member (the historical default).
    #[default]
    All,
    /// Notify only for messages that @mention the local user or reply to one
    /// of their messages.
    MentionsAndReplies,
    /// Never notify for this room.
    Muted,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Rooms {
    pub map: HashMap<VerifyingKey, RoomData>,
    #[serde(default)]
    pub current_room_key: Option<VerifyingKey>,
    /// Rooms the user has explicitly left. Persisted alongside `map` so
    /// the leave action survives across reloads and across legacy-delegate
    /// migrations. Without this set, `Rooms::merge` re-adds the room from
    /// any legacy delegate whose stored `rooms_data` still contained it
    /// (see freenet/river#247 — Ivvor's report 2026-05-14).
    ///
    /// Invariants:
    /// * A room key in `removed_rooms` MUST NOT also appear in `map`.
    ///   `Rooms::merge` enforces this defensively via `retain`.
    /// * On explicit rejoin (accepting an invitation or importing an
    ///   export of the room), the tombstone is cleared. The set is
    ///   therefore NOT strictly grow-only — it's a leave-on / rejoin-off
    ///   marker. Rejoin clears are local to the device that does the
    ///   rejoin; a second device that doesn't see the rejoin keeps the
    ///   tombstone and won't auto-re-add the room. Treated as acceptable
    ///   for the multi-device case because the leave was an explicit
    ///   user action on the original device; if the second device wants
    ///   the room back, it can rejoin explicitly there too.
    ///
    /// Sites that insert into `map` MUST also clear the corresponding
    /// entry from `removed_rooms` if the insert represents an explicit
    /// rejoin (see `members.rs` import-identity flow, `get_response.rs`
    /// invitation-accept flow). Sites that get state passively from the
    /// network (e.g. `UpdateNotification` handlers calling `get_mut`)
    /// should NOT clear the tombstone — the user explicitly left.
    #[serde(default)]
    pub removed_rooms: std::collections::HashSet<VerifyingKey>,
    /// Per-room notification preference (a local user setting), keyed by room
    /// owner key. An absent entry means [`NotificationMode::All`]. Persisted
    /// alongside `map` inside the `rooms_data` delegate blob — the same
    /// local-per-room-state pattern as `removed_rooms`, so it needs no new
    /// delegate storage key and survives delegate migration. A stale entry for
    /// a room no longer in `map` is harmless (never consulted) — but note a
    /// consequence: leaving a room does NOT clear its entry, so re-joining the
    /// same room later inherits the preference set before leaving (the merge
    /// below is local-wins and never overwrites a kept value).
    #[serde(default)]
    pub notification_modes: HashMap<VerifyingKey, NotificationMode>,
    /// Rooms whose contract key changed due to WASM update.
    /// Each entry is (owner_vk, old_contract_key) for rooms where the owner
    /// should send an upgrade pointer to the old contract.
    #[serde(skip)]
    pub migrated_rooms: Vec<(VerifyingKey, ContractKey)>,
}

impl PartialEq for Rooms {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
            && self.removed_rooms == other.removed_rooms
            && self.notification_modes == other.notification_modes
    }
}

impl Rooms {
    pub fn create_new_room_with_name(
        &mut self,
        self_sk: SigningKey,
        name: String,
        nickname: String,
        is_private: bool,
    ) -> VerifyingKey {
        use dioxus::logger::tracing::info;
        info!(
            "🟢 create_new_room_with_name called: name='{}', nickname='{}', is_private={}",
            name, nickname, is_private
        );

        let owner_vk = self_sk.verifying_key();
        let mut room_state = ChatRoomStateV1::default();

        // Generate room secret if private
        info!("🟢 Creating privacy mode and secrets...");
        let (privacy_mode, room_secret, room_secret_version) = if is_private {
            info!("🟢 Generating private room secret...");
            // Generate a random 32-byte secret
            let secret = crate::util::ecies::generate_room_secret();

            // Encrypt the secret for the owner using ECIES
            let (ciphertext, nonce, ephemeral_key) = encrypt_secret_for_member(&secret, &owner_vk);

            // Create the secret version record
            let secret_version = SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: get_current_system_time(),
            };

            let authorized_version = AuthorizedSecretVersionRecord::new(secret_version, &self_sk);

            // Create encrypted secret for the owner
            let encrypted_secret = EncryptedSecretForMemberV1 {
                member_id: owner_vk.into(),
                secret_version: 0,
                ciphertext,
                nonce,
                sender_ephemeral_public_key: ephemeral_key.to_bytes(),
                provider: owner_vk.into(),
            };

            let authorized_encrypted_secret =
                AuthorizedEncryptedSecretForMember::new(encrypted_secret, &self_sk);

            // Add to room state
            room_state.secrets.versions.push(authorized_version);
            room_state
                .secrets
                .encrypted_secrets
                .push(authorized_encrypted_secret);
            room_state.secrets.current_version = 0;

            info!("🟢 Private room secret generated and encrypted");
            (PrivacyMode::Private, Some(secret), Some(0u32))
        } else {
            info!("🟢 Public room, no secret needed");
            (PrivacyMode::Public, None, None)
        };

        // Set initial configuration with privacy mode
        info!("🟢 Creating configuration...");
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            privacy_mode,
            display: RoomDisplayMetadata {
                name: if let Some(ref secret) = room_secret {
                    // Encrypt room name for private rooms
                    use crate::util::ecies::encrypt_with_symmetric_key;
                    let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, name.as_bytes());
                    SealedBytes::Private {
                        ciphertext,
                        nonce,
                        secret_version: 0,
                        declared_len_bytes: name.len() as u32,
                    }
                } else {
                    SealedBytes::public(name.into_bytes())
                },
                description: None,
            },
            ..Configuration::default()
        };
        room_state.configuration = AuthorizedConfigurationV1::new(config, &self_sk);

        // Add owner to member_info
        let owner_info = MemberInfo {
            member_id: owner_vk.into(),
            version: 0,
            preferred_nickname: if let Some(ref secret) = room_secret {
                // Encrypt nickname for private rooms
                use crate::util::ecies::encrypt_with_symmetric_key;
                let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, nickname.as_bytes());
                SealedBytes::Private {
                    ciphertext,
                    nonce,
                    secret_version: 0,
                    declared_len_bytes: nickname.len() as u32,
                }
            } else {
                SealedBytes::public(nickname.into_bytes())
            },
        };
        let authorized_owner_info = AuthorizedMemberInfo::new(owner_info, &self_sk);
        room_state
            .member_info
            .member_info
            .push(authorized_owner_info);

        // Generate contract key for the room
        info!("🟢 Generating contract key...");
        let parameters = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&parameters);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        // Use the full ContractKey constructor that includes the code hash
        let contract_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);
        info!("🟢 Contract key generated: {:?}", contract_key);

        info!("🟢 Creating RoomData struct...");
        let secrets = if let Some(secret) = room_secret {
            let mut map = HashMap::new();
            map.insert(0, secret);
            map
        } else {
            HashMap::new()
        };
        let room_data = RoomData {
            owner_vk,
            room_state,
            self_sk,
            contract_key,
            last_read_message_id: None,
            secrets,
            current_secret_version: room_secret_version,
            last_secret_rotation: if room_secret_version.is_some() {
                Some(get_current_system_time())
            } else {
                None
            },
            key_migrated_to_delegate: false, // Will be checked/migrated on startup
            self_authorized_member: None,    // Owner doesn't need this
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        };

        info!("🟢 Inserting room into map...");
        self.map.insert(owner_vk, room_data);
        info!("🟢 create_new_room_with_name completed successfully, returning owner_vk");
        owner_vk
    }

    /// Merge the other Rooms into this Rooms (eg. when Rooms are loaded from storage)
    ///
    /// Union of `removed_rooms` tombstones: any room key the user has
    /// explicitly left on either side stays left. Rooms in `removed_rooms`
    /// are filtered out of the merge — that prevents a legacy delegate's
    /// stale `rooms_data` from re-adding a room the user has already
    /// removed (see freenet/river#247).
    pub fn merge(&mut self, other: Rooms) -> Result<(), String> {
        // Tombstones first: take the union before anything else, so the
        // filter below sees the combined set.
        for vk in other.removed_rooms {
            self.removed_rooms.insert(vk);
        }
        // Defensive: if a room ended up in both `map` and `removed_rooms`
        // (shouldn't happen — leave path adds to removed and removes from
        // map atomically), the tombstone wins.
        self.map.retain(|vk, _| !self.removed_rooms.contains(vk));

        // Notification preferences: keep this device's choice on conflict (a
        // local user setting), only adopting another source's value where this
        // device has none.
        for (vk, mode) in other.notification_modes {
            self.notification_modes.entry(vk).or_insert(mode);
        }

        for (vk, mut room_data) in other.map {
            // Honour tombstones — never re-add a room the user has left.
            if self.removed_rooms.contains(&vk) {
                continue;
            }

            // Capture the old contract key before regeneration
            let old_contract_key = room_data.contract_key;

            // Regenerate contract_key to ensure it matches the current bundled WASM
            // This handles the case where rooms were stored with an older WASM version
            room_data.regenerate_contract_key();

            // If the contract key changed (WASM was updated), track for upgrade pointer
            if old_contract_key != room_data.contract_key {
                self.migrated_rooms.push((vk, old_contract_key));
            }

            // If not already in the map, add the room
            if let std::collections::hash_map::Entry::Vacant(e) = self.map.entry(vk) {
                e.insert(room_data);
            } else {
                // If the room is already in the map, merge in the new data
                let self_room_data = self.map.get_mut(&vk).unwrap();
                if self_room_data.self_sk != room_data.self_sk {
                    return Err("self_sk is different".to_string());
                }
                self_room_data.room_state.merge(
                    &self_room_data.room_state.clone(),
                    &ChatRoomParametersV1 { owner: vk },
                    &room_data.room_state,
                )?;
            }
        }
        Ok(())
    }

    /// Mark a room as explicitly left. Removes from `map`, drops any
    /// pending upgrade-pointer entry in `migrated_rooms`, and adds the
    /// owner VK to `removed_rooms` so future merges don't re-add it.
    ///
    /// Idempotent — safe to call multiple times with the same key.
    pub fn leave_room(&mut self, room_vk: VerifyingKey) {
        self.map.remove(&room_vk);
        self.migrated_rooms.retain(|(vk, _)| vk != &room_vk);
        self.removed_rooms.insert(room_vk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::member::{AuthorizedMember, Member};

    #[test]
    fn notification_mode_defaults_to_all() {
        assert_eq!(NotificationMode::default(), NotificationMode::All);
    }

    #[test]
    fn merge_notification_modes_keeps_local_and_adopts_new() {
        let mut rng = rand::thread_rng();
        let shared = SigningKey::generate(&mut rng).verifying_key();
        let only_incoming = SigningKey::generate(&mut rng).verifying_key();

        let mut local = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: HashMap::new(),
            migrated_rooms: Vec::new(),
        };
        local
            .notification_modes
            .insert(shared, NotificationMode::Muted);

        let mut incoming = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: HashMap::new(),
            migrated_rooms: Vec::new(),
        };
        incoming
            .notification_modes
            .insert(shared, NotificationMode::All);
        incoming
            .notification_modes
            .insert(only_incoming, NotificationMode::MentionsAndReplies);

        local.merge(incoming).unwrap();

        // The local device's choice wins on conflict; a key present only in the
        // incoming set is adopted.
        assert_eq!(
            local.notification_modes.get(&shared),
            Some(&NotificationMode::Muted)
        );
        assert_eq!(
            local.notification_modes.get(&only_incoming),
            Some(&NotificationMode::MentionsAndReplies)
        );
    }

    /// Regression test for #85: accepting an invitation for a room that already
    /// exists in the ROOMS map must update self_sk so can_send_message() passes.
    #[test]
    fn test_can_send_message_after_self_sk_update() {
        let mut rng = rand::thread_rng();

        // Create owner
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        // Create room state with owner config
        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let mut room_state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };

        // Create an invitee and add them as a member
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();
        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: owner_vk.into(),
            member_vk: invitee_vk,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_sk);
        room_state.members.members.push(authorized_member);

        // Create RoomData with a STALE self_sk (different from the invitee key)
        let stale_sk = SigningKey::generate(&mut rng);
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

        let mut room_data = RoomData {
            owner_vk,
            room_state,
            self_sk: stale_sk,
            contract_key,
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        };

        // With stale key, user should NOT be recognized as a member
        assert_eq!(
            room_data.can_send_message(),
            Err(SendMessageError::UserNotMember)
        );

        // After updating self_sk to the invitee's key, user should be a member
        room_data.self_sk = invitee_sk;
        assert_eq!(room_data.can_send_message(), Ok(()));
    }

    /// Test that capture_self_membership_data captures and updates member_info.
    #[test]
    fn test_capture_self_membership_data_preserves_nickname() {
        use river_core::room_state::privacy::SealedBytes;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();
        let member_id = MemberId::from(&invitee_vk);

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let mut room_state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };

        // Add invitee as member
        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: owner_vk.into(),
            member_vk: invitee_vk,
        };
        room_state
            .members
            .members
            .push(AuthorizedMember::new(member, &owner_sk));

        // Add member_info with a custom nickname
        let info = MemberInfo {
            member_id,
            version: 0,
            preferred_nickname: SealedBytes::public("Alice".to_string().into_bytes()),
        };
        let authorized_info = AuthorizedMemberInfo::new_with_member_key(info, &invitee_sk);
        room_state.member_info.member_info.push(authorized_info);

        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

        let mut room_data = RoomData {
            owner_vk,
            room_state,
            self_sk: invitee_sk.clone(),
            contract_key,
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        };

        // Before capture, self_member_info should be None
        assert!(room_data.self_member_info.is_none());

        // Capture should populate self_member_info
        room_data.capture_self_membership_data(&params);
        assert!(room_data.self_member_info.is_some());
        let stored = room_data.self_member_info.as_ref().unwrap();
        assert_eq!(stored.member_info.member_id, member_id);
        assert_eq!(stored.member_info.version, 0);

        // Simulate nickname edit: update member_info in room_state with higher version
        let updated_info = MemberInfo {
            member_id,
            version: 1,
            preferred_nickname: SealedBytes::public("Bob".to_string().into_bytes()),
        };
        let updated_authorized =
            AuthorizedMemberInfo::new_with_member_key(updated_info, &invitee_sk);
        room_data.room_state.member_info.member_info[0] = updated_authorized;

        // Re-capture should update to latest version
        room_data.capture_self_membership_data(&params);
        let stored = room_data.self_member_info.as_ref().unwrap();
        assert_eq!(stored.member_info.version, 1);
    }

    #[test]
    fn test_is_awaiting_initial_sync() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_sk = SigningKey::generate(&mut rng);

        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

        // use_default_config=true simulates an imported room (config signed by zero key),
        // use_default_config=false simulates a created or synced room (config signed by owner).
        let make_room = |sk: SigningKey, use_default_config: bool| {
            let config = if use_default_config {
                AuthorizedConfigurationV1::default()
            } else {
                AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk)
            };
            let room_state = ChatRoomStateV1 {
                configuration: config,
                ..Default::default()
            };
            RoomData {
                owner_vk,
                room_state,
                self_sk: sk,
                contract_key,
                last_read_message_id: None,
                secrets: HashMap::new(),
                current_secret_version: None,
                last_secret_rotation: None,
                key_migrated_to_delegate: false,
                self_authorized_member: None,
                invite_chain: vec![],
                self_member_info: None,
                self_nickname: None,
                previous_contract_key: None,
                invitation_secrets: HashMap::new(),
            }
        };

        // Owner-created room (config signed by owner): NOT awaiting sync
        let owner_room = make_room(owner_sk.clone(), false);
        assert!(!owner_room.is_awaiting_initial_sync());

        // Owner import with default state (the bug case): IS awaiting sync
        // Previously this returned false due to owner bypass, causing signature failures
        let owner_imported = make_room(owner_sk.clone(), true);
        assert!(owner_imported.is_awaiting_initial_sync());

        // Non-owner import with default state: IS awaiting sync
        let imported_room = make_room(invitee_sk.clone(), true);
        assert!(imported_room.is_awaiting_initial_sync());

        // Non-owner synced room (config signed by owner): NOT awaiting sync
        let synced_room = make_room(invitee_sk.clone(), false);
        assert!(!synced_room.is_awaiting_initial_sync());
    }

    /// Helper to build a RoomData for rejoin tests.
    fn make_rejoin_test_room(
        owner_sk: &SigningKey,
        invitee_sk: &SigningKey,
        include_member: bool,
    ) -> RoomData {
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();

        let config = AuthorizedConfigurationV1::new(Configuration::default(), owner_sk);
        let mut room_state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };

        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: owner_vk.into(),
            member_vk: invitee_vk,
        };
        let authorized_member = AuthorizedMember::new(member, owner_sk);

        if include_member {
            room_state.members.members.push(authorized_member.clone());
        }

        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

        RoomData {
            owner_vk,
            room_state,
            self_sk: invitee_sk.clone(),
            contract_key,
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: Some(authorized_member),
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        }
    }

    /// freenet/river#345 headline: when one tab adds room A and another adds a
    /// DIFFERENT room B, merging the two snapshots must keep BOTH — the
    /// additive-union path of `Rooms::merge` (vacant-entry insert) that the
    /// chat-delegate CAS conflict-resolution relies on. Distinct from the
    /// tombstone tests, which cover the leave/remove direction.
    #[test]
    fn merge_preserves_distinct_rooms_from_both_sides() {
        let mut rng = rand::thread_rng();
        let owner_a = SigningKey::generate(&mut rng);
        let self_a = SigningKey::generate(&mut rng);
        let owner_b = SigningKey::generate(&mut rng);
        let self_b = SigningKey::generate(&mut rng);
        let vk_a = owner_a.verifying_key();
        let vk_b = owner_b.verifying_key();

        let mut local = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        local
            .map
            .insert(vk_a, make_rejoin_test_room(&owner_a, &self_a, true));

        let mut remote = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        remote
            .map
            .insert(vk_b, make_rejoin_test_room(&owner_b, &self_b, true));

        local
            .merge(remote)
            .expect("merge of distinct rooms should succeed");

        assert!(
            local.map.contains_key(&vk_a),
            "this tab's room A must survive"
        );
        assert!(
            local.map.contains_key(&vk_b),
            "the other tab's room B must be merged in, not clobbered"
        );
        assert_eq!(local.map.len(), 2);
    }

    #[test]
    fn test_build_rejoin_delta_returns_none_when_member_present() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        let (members, info) = room.build_rejoin_delta();
        assert!(members.is_none());
        assert!(info.is_none());
    }

    #[test]
    fn test_build_rejoin_delta_returns_none_for_owner() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);

        // Owner as self_sk, not in members list (owner is never explicitly listed)
        let mut room = make_rejoin_test_room(&owner_sk, &owner_sk, false);
        room.self_authorized_member = None;
        let (members, info) = room.build_rejoin_delta();
        assert!(members.is_none());
        assert!(info.is_none());
    }

    #[test]
    fn test_build_rejoin_delta_returns_none_without_credentials() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);
        room.self_authorized_member = None;
        let (members, info) = room.build_rejoin_delta();
        assert!(members.is_none());
        assert!(info.is_none());
    }

    #[test]
    fn test_build_rejoin_delta_constructs_delta_when_pruned() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();

        // Member NOT in members list but has stored credentials
        let room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);
        let (members, info) = room.build_rejoin_delta();

        let members = members.expect("should have members delta");
        assert_eq!(members.added().len(), 1);
        assert_eq!(members.added()[0].member.member_vk, invitee_vk);

        let info = info.expect("should have member_info delta");
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].member_info.member_id, MemberId::from(&invitee_vk));
    }

    #[test]
    fn build_rejoin_delta_uses_self_nickname_when_no_stored_member_info() {
        // A pruned member with no `self_member_info` but a recorded
        // `self_nickname` must re-add themselves under THAT nickname, not a
        // generic placeholder.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);
        room.self_member_info = None;
        room.self_nickname = Some("UserPicked".to_string());

        let (members, member_info) = room.build_rejoin_delta();
        assert!(members.is_some(), "pruned member must get a members delta");
        let member_info = member_info.expect("public room must produce member_info");
        let nickname =
            crate::util::ecies::unseal_bytes(&member_info[0].member_info.preferred_nickname, None)
                .expect("public-sealed nickname must unseal");
        assert_eq!(nickname, b"UserPicked");
    }

    #[test]
    fn build_rejoin_delta_private_room_seals_self_nickname() {
        // Private-room rejoin must Private-seal `self_nickname` with the
        // room secret — never publish it as plaintext.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_id: MemberId = owner_sk.verifying_key().into();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        room.self_member_info = None;
        room.self_nickname = Some("UserPicked".to_string());
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        // Treat the member as pruned: drop them from the members list but
        // keep the credentials needed to re-add.
        room.room_state.members.members.clear();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        room.self_authorized_member = Some(AuthorizedMember::new(member, &owner_sk));

        let (members, member_info) = room.build_rejoin_delta();
        assert!(members.is_some(), "pruned member must get a members delta");
        let member_info = member_info.expect("secret available → member_info sealed");
        assert!(
            matches!(
                member_info[0].member_info.preferred_nickname,
                SealedBytes::Private { .. }
            ),
            "private-room rejoin nickname must be Private-sealed, never plaintext"
        );
        let nickname = crate::util::ecies::unseal_bytes(
            &member_info[0].member_info.preferred_nickname,
            Some(&v0_secret),
        )
        .expect("sealed nickname must decrypt");
        assert_eq!(nickname, b"UserPicked");
    }

    #[test]
    fn build_rejoin_delta_private_room_defers_member_info_without_secret() {
        // Private room, member pruned, but the room secret is not available
        // locally to seal the nickname. The members delta must still re-add
        // the member, but member_info is deferred (None) rather than leaking
        // a plaintext nickname — the GET-path self-heal restores it later.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_id: MemberId = owner_sk.verifying_key().into();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        room.self_member_info = None;
        room.self_nickname = Some("UserPicked".to_string());
        // No secret available to seal with.
        room.secrets.clear();
        room.current_secret_version = None;
        room.room_state.members.members.clear();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        room.self_authorized_member = Some(AuthorizedMember::new(member, &owner_sk));

        let (members, member_info) = room.build_rejoin_delta();
        assert!(
            members.is_some(),
            "the member must still be re-added even when member_info is deferred"
        );
        assert!(
            member_info.is_none(),
            "no secret to seal → member_info deferred, never a plaintext leak"
        );
    }

    #[test]
    fn record_invite_credentials_stores_the_self_fields() {
        // `record_invite_credentials` must set all three `self_*` fields so
        // a caller cannot set `self_member_info` and forget `self_nickname`.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let owner_id: MemberId = owner_sk.verifying_key().into();

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);
        room.self_authorized_member = None;
        room.self_member_info = None;
        room.self_nickname = None;

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: invitee_sk.verifying_key(),
        };
        let authorized = AuthorizedMember::new(member, &owner_sk);

        room.record_invite_credentials(authorized.clone(), None, "Chosen".to_string());
        assert_eq!(room.self_authorized_member, Some(authorized));
        assert_eq!(
            room.self_member_info, None,
            "member_info passed as None (deferred private-room seal) stays None"
        );
        assert_eq!(room.self_nickname, Some("Chosen".to_string()));
    }

    #[test]
    fn build_rejoin_delta_private_room_ignores_public_sealed_stored_entry() {
        // A Public-sealed `self_member_info` (e.g. one left behind by a
        // public->private reconfiguration) must NOT be reused by the rejoin
        // path in a private room — that would republish a plaintext
        // nickname. The entry is rebuilt and Private-sealed instead.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_id: MemberId = owner_sk.verifying_key().into();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        room.self_nickname = Some("UserPicked".to_string());
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        let public_entry = MemberInfo {
            member_id: MemberId::from(&member_sk.verifying_key()),
            version: 2,
            preferred_nickname: SealedBytes::public(b"PlainLeak".to_vec()),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
            public_entry,
            &member_sk,
        ));
        room.room_state.members.members.clear();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        room.self_authorized_member = Some(AuthorizedMember::new(member, &owner_sk));

        let (_, member_info) = room.build_rejoin_delta();
        let member_info = member_info.expect("secret available → member_info rebuilt");
        assert!(
            matches!(
                member_info[0].member_info.preferred_nickname,
                SealedBytes::Private { .. }
            ),
            "a Public-sealed stored entry must not be reused in a private room"
        );
        let nickname = crate::util::ecies::unseal_bytes(
            &member_info[0].member_info.preferred_nickname,
            Some(&v0_secret),
        )
        .expect("rebuilt nickname must decrypt");
        assert_eq!(nickname, b"UserPicked");
    }

    #[test]
    fn build_rejoin_delta_private_room_reuses_private_sealed_stored_entry() {
        // A Private-sealed `self_member_info` IS reused verbatim by the
        // rejoin path — it already carries the correctly sealed nickname.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_id: MemberId = owner_sk.verifying_key().into();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        let private_entry = MemberInfo {
            member_id: MemberId::from(&member_sk.verifying_key()),
            version: 6,
            preferred_nickname: seal_bytes(b"SealedName", &v0_secret, 0),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
            private_entry,
            &member_sk,
        ));
        room.room_state.members.members.clear();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        room.self_authorized_member = Some(AuthorizedMember::new(member, &owner_sk));

        let (_, member_info) = room.build_rejoin_delta();
        let member_info = member_info.expect("should reuse the stored member_info");
        assert_eq!(
            member_info[0].member_info.version, 6,
            "a Private-sealed stored entry is reused verbatim"
        );
    }

    #[test]
    fn record_self_nickname_edit_updates_self_fields_for_self() {
        // Editing the local user's own nickname updates BOTH cached fields
        // so the heal/rejoin paths republish the edit, not the prior value.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let self_member_id = MemberId::from(&invitee_sk.verifying_key());

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        room.self_member_info = None;
        room.self_nickname = None;

        let edited = MemberInfo {
            member_id: self_member_id,
            version: 2,
            preferred_nickname: SealedBytes::public(b"Edited".to_vec()),
        };
        let edited = AuthorizedMemberInfo::new_with_member_key(edited, &invitee_sk);

        room.record_self_nickname_edit(self_member_id, edited.clone(), "Edited".to_string());
        assert_eq!(room.self_member_info, Some(edited));
        assert_eq!(room.self_nickname, Some("Edited".to_string()));
    }

    #[test]
    fn record_self_nickname_edit_ignores_edits_to_other_members() {
        // Editing someone else's nickname must NOT touch our cached fields.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let other_sk = SigningKey::generate(&mut rng);
        let other_member_id = MemberId::from(&other_sk.verifying_key());

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        room.self_member_info = None;
        room.self_nickname = Some("Mine".to_string());

        let other = MemberInfo {
            member_id: other_member_id,
            version: 1,
            preferred_nickname: SealedBytes::public(b"Other".to_vec()),
        };
        let other = AuthorizedMemberInfo::new_with_member_key(other, &other_sk);

        room.record_self_nickname_edit(other_member_id, other, "Other".to_string());
        assert_eq!(
            room.self_member_info, None,
            "another member's edit must not touch ours"
        );
        assert_eq!(room.self_nickname, Some("Mine".to_string()));
    }

    #[test]
    fn test_build_rejoin_delta_uses_stored_member_info() {
        use river_core::room_state::privacy::SealedBytes;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();
        let member_id = MemberId::from(&invitee_vk);

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);

        // Store member_info with a custom nickname
        let info = MemberInfo {
            member_id,
            version: 5,
            preferred_nickname: SealedBytes::public("Alice".to_string().into_bytes()),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(info, &invitee_sk));

        let (_, member_info) = room.build_rejoin_delta();
        let member_info = member_info.expect("should have member_info delta");
        assert_eq!(member_info[0].member_info.version, 5);
    }

    #[test]
    fn test_build_rejoin_delta_includes_missing_invite_chain() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let chain_sk = SigningKey::generate(&mut rng);
        let chain_vk = chain_sk.verifying_key();

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);

        // Add a chain member that's also missing from the room
        let chain_member = Member {
            owner_member_id: owner_sk.verifying_key().into(),
            invited_by: owner_sk.verifying_key().into(),
            member_vk: chain_vk,
        };
        room.invite_chain
            .push(AuthorizedMember::new(chain_member, &owner_sk));

        let (members, _) = room.build_rejoin_delta();
        let members = members.expect("should have members delta");
        // Should include both self and the missing chain member
        assert_eq!(members.added().len(), 2);
    }

    // --- build_member_info_heal: PR #272 "Unknown member" remediation ---

    #[test]
    fn build_member_info_heal_returns_some_when_stranded() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();

        // Network state: invitee is in `members` but has no member_info.
        let room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        let network_state = room.room_state.clone();

        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded member must produce a heal entry");
        assert_eq!(heal.member_info.member_id, MemberId::from(&invitee_vk));
        assert_eq!(heal.member_info.version, 0);
        // The healed entry must be a valid self-signed AuthorizedMemberInfo
        // — the room contract only accepts member_info self-signed by the
        // member's own key. A heal signed with the wrong key would be
        // silently rejected by the contract.
        heal.verify_signature_with_key(&invitee_vk)
            .expect("healed entry must be self-signed by the member");
    }

    #[test]
    fn build_member_info_heal_returns_none_when_member_info_present() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();

        let room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        let mut network_state = room.room_state.clone();
        // Network already carries the invitee's member_info — not stranded.
        let info = MemberInfo {
            member_id: MemberId::from(&invitee_vk),
            version: 0,
            preferred_nickname: SealedBytes::public(b"Present".to_vec()),
        };
        network_state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(info, &invitee_sk));

        assert!(room.build_member_info_heal(&network_state).is_none());
    }

    #[test]
    fn build_member_info_heal_returns_none_for_owner() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        // self_sk == owner_sk — the owner's member_info is managed separately.
        let room = make_rejoin_test_room(&owner_sk, &owner_sk, false);
        let network_state = room.room_state.clone();
        assert!(room.build_member_info_heal(&network_state).is_none());
    }

    #[test]
    fn build_member_info_heal_returns_none_when_not_a_member() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        // include_member = false — invitee is absent from the network state.
        let room = make_rejoin_test_room(&owner_sk, &invitee_sk, false);
        let network_state = room.room_state.clone();
        assert!(room.build_member_info_heal(&network_state).is_none());
    }

    #[test]
    fn build_member_info_heal_prefers_stored_self_member_info() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let member_id = MemberId::from(&invitee_sk.verifying_key());

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        // The user already picked a nickname — the heal must preserve it
        // rather than minting a fresh default handle.
        let stored = MemberInfo {
            member_id,
            version: 7,
            preferred_nickname: SealedBytes::public(b"ChosenName".to_vec()),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
            stored,
            &invitee_sk,
        ));

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded member must produce a heal entry");
        assert_eq!(heal.member_info.version, 7);
    }

    #[test]
    fn build_member_info_heal_private_room_seals_when_secret_available() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        // Private room; `member_sk` is in `members` with no member_info
        // (stranded). self is the member.
        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        // The heal reads the secret from the network `state` it is given,
        // so that state must carry an encrypted-secret blob for the
        // member (the heal does NOT trust `self.secrets`).
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &member_sk.verifying_key(),
            &v0_secret,
            0,
        );

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded private-room member with a secret must heal");
        // The nickname MUST be encrypted — never a plaintext Public seal.
        assert!(
            matches!(
                heal.member_info.preferred_nickname,
                SealedBytes::Private { .. }
            ),
            "private-room heal must produce a Private-sealed nickname"
        );
        heal.verify_signature_with_key(&member_sk.verifying_key())
            .expect("healed entry must be self-signed by the member");
    }

    #[test]
    fn build_member_info_heal_private_room_defers_without_secret() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        // Private room, member stranded — but the network state carries
        // NO encrypted-secret blob for the member (the owner's back-fill
        // has not arrived yet). make_private_owner_room seeds only
        // `secrets.versions`, not `encrypted_secrets`.
        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();

        let network_state = room.room_state.clone();
        // No secret in `state` to seal the nickname → the heal must defer
        // (return None) rather than publish a plaintext nickname into a
        // private room. The member stays "Unknown" until a later GET.
        assert!(
            room.build_member_info_heal(&network_state).is_none(),
            "private-room heal must defer when the room secret is unavailable"
        );
    }

    #[test]
    fn build_member_info_heal_private_room_uses_network_privacy_not_local_placeholder() {
        // Regression for the round-2 review (Codex P1 / skeptical H1):
        // an imported room's LOCAL room_state is a public placeholder, so
        // the heal must read privacy mode from the network `state`, not
        // from `self`. With a public local placeholder but a PRIVATE
        // network state and no secret blob, the heal must DEFER — never
        // mint a plaintext Public-sealed nickname.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        // `self` carries a public placeholder room_state...
        let mut room = make_rejoin_test_room(&owner_sk, &member_sk, true);
        assert!(!room.is_private(), "local placeholder is public");
        room.self_member_info = None;

        // ...but the network state is a PRIVATE room with the member
        // stranded and no secret blob.
        let private_state = make_private_owner_room(&owner_sk, &member_sk).room_state;

        assert!(
            room.build_member_info_heal(&private_state).is_none(),
            "heal must read privacy from the network state and defer — \
             trusting the local public placeholder would leak a plaintext \
             nickname into a private room"
        );
    }

    #[test]
    fn build_member_info_heal_private_room_ignores_public_sealed_stored_entry() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &member_sk.verifying_key(),
            &v0_secret,
            0,
        );

        // A stored entry whose nickname is PUBLIC-sealed must NOT be
        // reused in a private room — reusing it would publish plaintext.
        let public_entry = MemberInfo {
            member_id: MemberId::from(&member_sk.verifying_key()),
            version: 3,
            preferred_nickname: SealedBytes::public(b"PlainName".to_vec()),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
            public_entry,
            &member_sk,
        ));
        // Having skipped the public-sealed entry, the heal must fall through
        // to `self_nickname` — not a generated default.
        room.self_nickname = Some("FellThrough".to_string());

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("private-room member with a secret must heal");
        assert!(
            matches!(
                heal.member_info.preferred_nickname,
                SealedBytes::Private { .. }
            ),
            "a Public-sealed stored entry must not be reused in a private room"
        );
        let nickname = crate::util::ecies::unseal_bytes(
            &heal.member_info.preferred_nickname,
            Some(&v0_secret),
        )
        .expect("sealed nickname must decrypt");
        assert_eq!(
            nickname, b"FellThrough",
            "after skipping the public-sealed entry the heal must seal self_nickname"
        );
    }

    #[test]
    fn build_member_info_heal_reads_secret_from_state_not_self() {
        // Pins the secret-SOURCE half of the round-3 fix: the heal must
        // derive the room secret from the network `state`, NOT from
        // `self.get_secret()`. For an imported room `self.secrets` is
        // empty at heal-build time (it is repopulated later in a deferred
        // closure) while the secret blob lives in the fetched `state`.
        // Reverting the heal to `self.get_secret()` makes this test fail.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &member_sk.verifying_key(),
            &v0_secret,
            0,
        );
        let network_state = room.room_state.clone();
        // Imported-room reality: self.secrets is empty; the secret lives
        // only in the network `state`.
        room.secrets.clear();
        room.current_secret_version = None;
        assert!(room.get_secret().is_none());

        let heal = room
            .build_member_info_heal(&network_state)
            .expect("heal must seal using the secret from the network state");
        assert!(matches!(
            heal.member_info.preferred_nickname,
            SealedBytes::Private { .. }
        ));
    }

    #[test]
    fn build_member_info_heal_uses_self_nickname_when_member_info_deferred() {
        // Regression for the DM-invite "auto-nickname ignores user override"
        // bug. A join whose member_info build was deferred has no
        // `self_member_info`, but the nickname the user typed is retained in
        // `self_nickname`. The heal must publish THAT — not a generated
        // default handle — or the user's choice is silently overridden.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        room.self_member_info = None;
        room.self_nickname = Some("UserPicked".to_string());

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded member must produce a heal entry");
        let nickname = crate::util::ecies::unseal_bytes(&heal.member_info.preferred_nickname, None)
            .expect("public-sealed nickname must unseal");
        assert_eq!(
            nickname, b"UserPicked",
            "heal must publish the user's chosen nickname, not a generated default"
        );
    }

    #[test]
    fn build_member_info_heal_private_room_seals_self_nickname() {
        // Private-room half of the DM-invite override bug: when the room
        // secret was unavailable at join time the member_info is deferred,
        // so `self_member_info` is None. The heal must seal the user's
        // chosen `self_nickname` — not a generated default.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        room.self_member_info = None;
        room.self_nickname = Some("UserPicked".to_string());
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &member_sk.verifying_key(),
            &v0_secret,
            0,
        );

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded private-room member with a secret must heal");
        assert!(
            matches!(
                heal.member_info.preferred_nickname,
                SealedBytes::Private { .. }
            ),
            "private-room heal must Private-seal the nickname"
        );
        let nickname = crate::util::ecies::unseal_bytes(
            &heal.member_info.preferred_nickname,
            Some(&v0_secret),
        )
        .expect("sealed nickname must decrypt with the room secret");
        assert_eq!(
            nickname, b"UserPicked",
            "private-room heal must seal the user's chosen nickname, not a default"
        );
    }

    #[test]
    fn build_member_info_heal_falls_back_to_default_without_self_nickname() {
        // When neither `self_member_info` nor `self_nickname` is set (an
        // imported room, or a room joined before `self_nickname` existed),
        // the heal still mints a deterministic default handle.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        room.self_member_info = None;
        room.self_nickname = None;

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded member must produce a heal entry");
        let nickname = crate::util::ecies::unseal_bytes(&heal.member_info.preferred_nickname, None)
            .expect("public-sealed nickname must unseal");
        assert_eq!(
            nickname,
            crate::nickname::generate_default_nickname(&invitee_sk.verifying_key()).into_bytes(),
            "with no recorded nickname the heal must use the generated default"
        );
    }

    #[test]
    fn build_member_info_heal_self_member_info_outranks_self_nickname() {
        // Priority order: a published `self_member_info` must win over
        // `self_nickname`. Otherwise a stale join-time `self_nickname` could
        // override a nickname the user later changed and published.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let member_id = MemberId::from(&invitee_sk.verifying_key());

        let mut room = make_rejoin_test_room(&owner_sk, &invitee_sk, true);
        room.self_nickname = Some("JoinTimeName".to_string());
        let stored = MemberInfo {
            member_id,
            version: 9,
            preferred_nickname: SealedBytes::public(b"PublishedName".to_vec()),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
            stored,
            &invitee_sk,
        ));

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded member must produce a heal entry");
        let nickname = crate::util::ecies::unseal_bytes(&heal.member_info.preferred_nickname, None)
            .expect("public-sealed nickname must unseal");
        assert_eq!(
            nickname, b"PublishedName",
            "published self_member_info must outrank the join-time self_nickname"
        );
    }

    #[test]
    fn build_member_info_heal_private_room_self_member_info_outranks_self_nickname() {
        // Private-room priority: a Private-sealed `self_member_info` must be
        // reused even when `self_nickname` is also set.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.self_sk = member_sk.clone();
        room.self_nickname = Some("JoinTimeName".to_string());
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &member_sk.verifying_key(),
            &v0_secret,
            0,
        );
        let stored_info = MemberInfo {
            member_id: MemberId::from(&member_sk.verifying_key()),
            version: 4,
            preferred_nickname: seal_bytes(b"PublishedName", &v0_secret, 0),
        };
        room.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
            stored_info,
            &member_sk,
        ));

        let network_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&network_state)
            .expect("stranded private-room member must heal");
        assert_eq!(
            heal.member_info.version, 4,
            "the Private-sealed self_member_info must be reused verbatim"
        );
        let nickname = crate::util::ecies::unseal_bytes(
            &heal.member_info.preferred_nickname,
            Some(&v0_secret),
        )
        .expect("sealed nickname must decrypt");
        assert_eq!(nickname, b"PublishedName");
    }

    #[test]
    fn current_secret_from_state_none_without_blob() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        // A public room (default config) carries no encrypted_secrets, so
        // the helper returns None and the invitation-accept path
        // public-seals the nickname (correct for a public room).
        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };
        assert!(
            current_secret_from_state(&state, &member_sk).is_none(),
            "no encrypted_secrets blob for the member → None"
        );
    }

    #[test]
    fn current_secret_from_state_decrypts_blob() {
        // Success path: a private room state carrying an encrypted-secret
        // blob for the member yields the decrypted secret + version.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        let v0_secret = *room.secrets.get(&0).expect("v0 secret seeded");
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &member_sk.verifying_key(),
            &v0_secret,
            0,
        );

        let (secret, version) = current_secret_from_state(&room.room_state, &member_sk)
            .expect("blob present for the member → decrypts");
        assert_eq!(version, 0);
        assert_eq!(secret, v0_secret);
    }

    /// Builds a private owner-mode room with one invited member, populated
    /// with a v0 secret. Used as a fixture for rotation tests.
    fn make_private_owner_room(owner_sk: &SigningKey, member_sk: &SigningKey) -> RoomData {
        let owner_vk = owner_sk.verifying_key();
        let member_vk = member_sk.verifying_key();
        let owner_id: MemberId = owner_vk.into();

        let mut config = Configuration {
            owner_member_id: owner_id,
            privacy_mode: PrivacyMode::Private,
            ..Configuration::default()
        };
        config.configuration_version = 1;

        let mut room_state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, owner_sk),
            ..Default::default()
        };

        // Add member.
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        room_state
            .members
            .members
            .push(AuthorizedMember::new(member, owner_sk));

        // Seed v0 secret as the deterministic value.
        let v0_secret =
            river_core::key_derivation::derive_room_secret(&owner_sk.to_bytes(), &owner_vk, 0);
        let v0_record = SecretVersionRecordV1 {
            version: 0,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: get_current_system_time(),
        };
        room_state
            .secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(v0_record, owner_sk));
        room_state.secrets.current_version = 0;

        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key =
            ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

        let mut secrets = HashMap::new();
        secrets.insert(0u32, v0_secret);

        RoomData {
            owner_vk,
            room_state,
            self_sk: owner_sk.clone(),
            contract_key,
            last_read_message_id: None,
            secrets,
            current_secret_version: Some(0),
            last_secret_rotation: Some(get_current_system_time()),
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        }
    }

    /// Regression test for freenet/river#310: in a private room, an edited
    /// message must NOT briefly revert to its original text when a new
    /// message (or any other local action) is sent.
    ///
    /// Root cause: the local optimistic send/edit/delete/react handlers call
    /// `ChatRoomStateV1::apply_delta`, whose `MessagesV1` impl ends with the
    /// non-decrypting `rebuild_actions_state()`. For a private room that can
    /// only decode PUBLIC actions, so it wipes the edit (carried by a private
    /// action message) from `actions_state.edited_content` until the network
    /// echo runs the decrypt-aware rebuild. `RoomData::rebuild_private_actions_state`
    /// restores it synchronously after the optimistic apply.
    ///
    /// This test reproduces the bug (asserts `apply_delta` alone drops the
    /// edit) and verifies the fix (the edit survives once the helper runs).
    #[test]
    fn private_edit_survives_new_message_send() {
        use river_core::room_state::content::{
            ActionContentV1, TextContentV1, CONTENT_TYPE_TEXT, TEXT_CONTENT_VERSION,
        };
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
        use river_core::room_state::ChatRoomStateV1Delta;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id: MemberId = owner_vk.into();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let (secret, secret_version) = {
            let (s, v) = room.get_secret().expect("private room must have a secret");
            (*s, v)
        };

        // 1. Author an original message (owner-authored, encrypted).
        let original_text = "Original content";
        let (orig_ct, orig_nonce) = crate::util::ecies::encrypt_with_symmetric_key(
            &secret,
            &TextContentV1::new(original_text.to_string()).encode(),
        );
        let original_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: get_current_system_time(),
            content: RoomMessageBody::private(
                CONTENT_TYPE_TEXT,
                TEXT_CONTENT_VERSION,
                orig_ct,
                orig_nonce,
                secret_version,
            ),
        };
        let auth_original = AuthorizedMessageV1::new(original_msg, &owner_sk);
        let original_id = auth_original.id();

        // 2. Author an edit action (private, encrypted) for that message.
        let edited_text = "Edited content";
        let edit_action = ActionContentV1::edit(original_id.clone(), edited_text.to_string());
        let (edit_ct, edit_nonce) =
            crate::util::ecies::encrypt_with_symmetric_key(&secret, &edit_action.encode());
        let edit_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: get_current_system_time() + std::time::Duration::from_secs(1),
            content: RoomMessageBody::private_action(edit_ct, edit_nonce, secret_version),
        };
        let auth_edit = AuthorizedMessageV1::new(edit_msg, &owner_sk);

        // Apply original + edit, then rebuild with decryption (mirrors the
        // network ingestion path). The edit must now be visible.
        room.room_state
            .apply_delta(
                &ChatRoomStateV1::default(),
                &params,
                &Some(ChatRoomStateV1Delta {
                    recent_messages: Some(vec![auth_original.clone(), auth_edit]),
                    ..Default::default()
                }),
            )
            .expect("applying original + edit must succeed");
        room.rebuild_private_actions_state();
        assert_eq!(
            room.room_state
                .recent_messages
                .effective_text(&auth_original),
            Some(edited_text.to_string()),
            "edit must be applied before the new message is sent"
        );

        // 3. Send a NEW message — the optimistic path runs apply_delta, whose
        //    private-room rebuild is public-only.
        let (new_ct, new_nonce) = crate::util::ecies::encrypt_with_symmetric_key(
            &secret,
            &TextContentV1::new("A brand new message".to_string()).encode(),
        );
        let new_msg = MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: get_current_system_time() + std::time::Duration::from_secs(2),
            content: RoomMessageBody::private(
                CONTENT_TYPE_TEXT,
                TEXT_CONTENT_VERSION,
                new_ct,
                new_nonce,
                secret_version,
            ),
        };
        let auth_new = AuthorizedMessageV1::new(new_msg, &owner_sk);
        room.room_state
            .apply_delta(
                &ChatRoomStateV1::default(),
                &params,
                &Some(ChatRoomStateV1Delta {
                    recent_messages: Some(vec![auth_new]),
                    ..Default::default()
                }),
            )
            .expect("applying new message must succeed");

        // BUG REPRODUCTION: immediately after apply_delta (before the fix's
        // helper runs), the private edit has been wiped from actions_state.
        // The conversation render does
        //   `effective_text(msg).unwrap_or_else(|| decrypt original ciphertext)`
        // (see conversation.rs:210-214), so with the edit gone, `effective_text`
        // returns `None` and the UI falls back to decrypting the ORIGINAL
        // ciphertext — i.e. the message visibly reverts to its pre-edit text.
        // That is exactly the transient flicker #310 describes.
        assert!(
            !room.room_state.recent_messages.is_edited(&original_id),
            "sanity: apply_delta's public-only rebuild drops the private edit \
             (this is the bug being fixed)"
        );
        assert_eq!(
            room.room_state
                .recent_messages
                .effective_text(&auth_original),
            None,
            "with the edit wiped, effective_text falls through to decrypting \
             the original ciphertext — the UI shows the pre-edit text"
        );

        // FIX: re-derive the private actions_state synchronously, as the
        // optimistic handlers now do. The edit is restored — no flicker.
        room.rebuild_private_actions_state();
        assert_eq!(
            room.room_state
                .recent_messages
                .effective_text(&auth_original),
            Some(edited_text.to_string()),
            "rebuild_private_actions_state must restore the edit after the \
             optimistic new-message apply_delta (#310)"
        );

        // Broader class (#310): the wipe is NOT specific to message deltas.
        // `MessagesV1::apply_delta` ALWAYS ends with the public-only
        // rebuild_actions_state, even when the delta carries no
        // `recent_messages` at all — so sending a DM, editing a nickname,
        // or banning a member (member/member_info/direct_messages/secrets
        // deltas) wipes the private edit too. Verify an empty (non-message)
        // delta reproduces the wipe and that the helper restores it.
        room.room_state
            .apply_delta(
                &ChatRoomStateV1::default(),
                &params,
                &Some(ChatRoomStateV1Delta::default()),
            )
            .expect("applying an empty delta must succeed");
        assert!(
            !room.room_state.recent_messages.is_edited(&original_id),
            "an apply_delta with no recent_messages still wipes the private \
             edit — this is why DM/nickname/ban paths also revert (#310)"
        );
        room.rebuild_private_actions_state();
        assert_eq!(
            room.room_state
                .recent_messages
                .effective_text(&auth_original),
            Some(edited_text.to_string()),
            "rebuild_private_actions_state must restore the edit after ANY \
             optimistic apply_delta, not just message sends (#310)"
        );
    }

    /// Fix 1 (#228 PR 2 v2): UI-side `rotate_secret` derives the new
    /// secret deterministically via `key_derivation::derive_room_secret`,
    /// so two replicas (UI + delegate, or two devices) produce the
    /// same byte value for the new secret.
    #[test]
    fn ui_rotation_uses_deterministic_derivation() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);

        // Capture pre-rotation state.
        let pre_version = room.room_state.secrets.current_version;
        assert_eq!(pre_version, 0);

        // Rotate.
        let _delta = room
            .rotate_secret()
            .expect("rotation must succeed for private owner room");

        // The new secret must equal the deterministic derivation.
        let expected = river_core::key_derivation::derive_room_secret(
            &owner_sk.to_bytes(),
            &owner_vk,
            pre_version + 1,
        );
        let (actual, version) = room.get_secret().expect("post-rotation secret must exist");
        assert_eq!(version, pre_version + 1);
        assert_eq!(*actual, expected);
    }

    /// Both the UI rotate_secret and the delegate's rotation pipeline
    /// (which both call `derive_room_secret`) produce byte-identical
    /// secrets for the same `(owner_seed, owner_vk, version)`. Concurrent
    /// rotation by both paths therefore converges via the contract's
    /// duplicate-version dedup.
    #[test]
    fn ui_and_delegate_rotation_produce_identical_secrets() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.rotate_secret().expect("rotation must succeed");

        let (ui_secret, ui_version) = {
            let (s, v) = room.get_secret().unwrap();
            (*s, v)
        };
        assert_eq!(ui_version, 1);

        // The delegate's rotation pipeline calls
        // `derive_room_secret(&signing_key_seed, &owner_vk, new_version)`
        // for the same `new_version`. With identical inputs across both
        // paths, the secret must match byte-for-byte.
        let delegate_secret =
            river_core::key_derivation::derive_room_secret(&owner_sk.to_bytes(), &owner_vk, 1);
        assert_eq!(ui_secret, delegate_secret);
    }

    /// Regression test for freenet/river#247: leaving a room must add it
    /// to `removed_rooms` so subsequent merges (e.g. legacy-delegate
    /// migration) don't silently re-add the room.
    ///
    /// Scenario:
    /// 1. User has room R in `map`.
    /// 2. User leaves R via `leave_room(R)` — should remove from `map`
    ///    AND add R's owner VK to `removed_rooms`.
    /// 3. On reload, the legacy migration produces a `Rooms` value that
    ///    still contains R (because legacy delegate's stored rooms_data
    ///    was never purged when superseded).
    /// 4. `current_rooms.merge(legacy_rooms)` must NOT re-add R.
    #[test]
    fn leave_room_tombstone_prevents_merge_re_adding() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let room_data = {
            let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
            let mut state = ChatRoomStateV1 {
                configuration: config,
                ..Default::default()
            };
            let member = Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: member_sk.verifying_key(),
            };
            state
                .members
                .members
                .push(AuthorizedMember::new(member, &owner_sk));
            let params = ChatRoomParametersV1 { owner: owner_vk };
            let params_bytes = to_cbor_vec(&params);
            let contract_key = ContractKey::from_params_and_code(
                Parameters::from(params_bytes),
                &ContractCode::from(ROOM_CONTRACT_WASM),
            );
            RoomData {
                owner_vk,
                room_state: state,
                self_sk: member_sk.clone(),
                contract_key,
                last_read_message_id: None,
                secrets: HashMap::new(),
                current_secret_version: None,
                last_secret_rotation: None,
                key_migrated_to_delegate: false,
                self_authorized_member: None,
                invite_chain: vec![],
                self_member_info: None,
                self_nickname: None,
                previous_contract_key: None,
                invitation_secrets: HashMap::new(),
            }
        };

        let mut current = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        current.map.insert(owner_vk, room_data.clone());

        // Step 2: leave the room.
        current.leave_room(owner_vk);
        assert!(!current.map.contains_key(&owner_vk));
        assert!(current.removed_rooms.contains(&owner_vk));

        // Step 3: legacy-delegate snapshot still has the room.
        let mut legacy = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        legacy.map.insert(owner_vk, room_data);

        // Step 4: merge must not re-add.
        current.merge(legacy).expect("merge");
        assert!(
            !current.map.contains_key(&owner_vk),
            "merge must respect the removed_rooms tombstone"
        );
        assert!(
            current.removed_rooms.contains(&owner_vk),
            "tombstone must survive the merge"
        );
    }

    /// Backward-compat: a `rooms_data` blob serialised before this PR
    /// does not contain the `removed_rooms` field. `#[serde(default)]`
    /// must populate it as an empty set so existing users' delegate
    /// state deserialises cleanly on first load post-deploy.
    #[test]
    fn rooms_deserialises_pre_247_blob_with_default_removed_rooms() {
        // Pre-#247 shape: just `map` and `current_room_key`. Build a
        // ciborium-encoded representation by hand. Map type 0xa2 = 2
        // entries; text "map" → empty map; text "current_room_key" → null.
        // Equivalent to ciborium-encoding a small adhoc serde struct.
        #[derive(Serialize)]
        struct LegacyRooms {
            map: HashMap<VerifyingKey, RoomData>,
            current_room_key: Option<VerifyingKey>,
        }
        let legacy = LegacyRooms {
            map: HashMap::new(),
            current_room_key: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut buf).unwrap();
        let decoded: Rooms = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert!(decoded.removed_rooms.is_empty());
        assert!(decoded.map.is_empty());
        assert!(decoded.current_room_key.is_none());
    }

    /// Round-trip: serialise a `Rooms` containing a tombstone, deserialise,
    /// and verify the tombstone survives. This pins the wire-format for
    /// the new field so a future serde rename / shape change can't drop
    /// the tombstone silently.
    #[test]
    fn rooms_round_trip_preserves_removed_rooms_tombstone() {
        let mut rng = rand::thread_rng();
        let vk = SigningKey::generate(&mut rng).verifying_key();
        let mut original = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        original.removed_rooms.insert(vk);

        let mut buf: Vec<u8> = Vec::new();
        ciborium::ser::into_writer(&original, &mut buf).unwrap();
        let decoded: Rooms = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert!(decoded.removed_rooms.contains(&vk));
    }

    /// Tombstone-clear semantics: an empty `removed_rooms` is implicit
    /// for new rooms; a tombstoned key that's re-cleared (e.g. by
    /// invitation accept) must NOT block the next merge.
    #[test]
    fn cleared_tombstone_allows_merge_to_re_add() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let room_data = {
            let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
            let state = ChatRoomStateV1 {
                configuration: config,
                ..Default::default()
            };
            let params = ChatRoomParametersV1 { owner: owner_vk };
            let params_bytes = to_cbor_vec(&params);
            let contract_key = ContractKey::from_params_and_code(
                Parameters::from(params_bytes),
                &ContractCode::from(ROOM_CONTRACT_WASM),
            );
            RoomData {
                owner_vk,
                room_state: state,
                self_sk: member_sk,
                contract_key,
                last_read_message_id: None,
                secrets: HashMap::new(),
                current_secret_version: None,
                last_secret_rotation: None,
                key_migrated_to_delegate: false,
                self_authorized_member: None,
                invite_chain: vec![],
                self_member_info: None,
                self_nickname: None,
                previous_contract_key: None,
                invitation_secrets: HashMap::new(),
            }
        };

        let mut current = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        // Step 1: leave the room.
        current.leave_room(owner_vk);
        // Step 2: simulate explicit rejoin clearing the tombstone (what
        // the invitation-accept and import-identity paths do).
        current.removed_rooms.remove(&owner_vk);
        // Step 3: merge an incoming snapshot that has the room.
        let mut incoming = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        incoming.map.insert(owner_vk, room_data);
        current.merge(incoming).expect("merge");
        // Room must be back since the tombstone was cleared.
        assert!(current.map.contains_key(&owner_vk));
        assert!(!current.removed_rooms.contains(&owner_vk));
    }

    /// Merge unions tombstones from both sides — if EITHER side has the
    /// room in `removed_rooms`, it must stay out (and any matching
    /// `map` entry on the receiver side is dropped).
    #[test]
    fn merge_unions_removed_rooms_tombstones() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let member_sk = SigningKey::generate(&mut rng);

        let room_data = {
            let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
            let state = ChatRoomStateV1 {
                configuration: config,
                ..Default::default()
            };
            let params = ChatRoomParametersV1 { owner: owner_vk };
            let params_bytes = to_cbor_vec(&params);
            let contract_key = ContractKey::from_params_and_code(
                Parameters::from(params_bytes),
                &ContractCode::from(ROOM_CONTRACT_WASM),
            );
            RoomData {
                owner_vk,
                room_state: state,
                self_sk: member_sk,
                contract_key,
                last_read_message_id: None,
                secrets: HashMap::new(),
                current_secret_version: None,
                last_secret_rotation: None,
                key_migrated_to_delegate: false,
                self_authorized_member: None,
                invite_chain: vec![],
                self_member_info: None,
                self_nickname: None,
                previous_contract_key: None,
                invitation_secrets: HashMap::new(),
            }
        };

        // Receiver has the room in `map`. Sender has it in `removed_rooms`.
        // After merge, neither side has it.
        let mut receiver = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        receiver.map.insert(owner_vk, room_data);
        let mut sender = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            notification_modes: Default::default(),
            migrated_rooms: Vec::new(),
        };
        sender.removed_rooms.insert(owner_vk);

        receiver.merge(sender).expect("merge");
        assert!(!receiver.map.contains_key(&owner_vk));
        assert!(receiver.removed_rooms.contains(&owner_vk));
    }

    /// Rotation refuses to wrap when the current version is `u32::MAX`,
    /// matching the same guard in the delegate pipeline (Fix 9).
    /// Regression test for Bug #3 PR B (Ivvor 2026-05-17): the UI
    /// synchronous fast-path used on ban / manual-rotate must back-fill
    /// prior-version blobs for any current member who lacks them in the
    /// room state. Before PR B the UI only emitted blobs at `new_version`,
    /// so a freshly-joined invitee who arrived between rotations could
    /// never recover v0 to decrypt the room name / pre-join messages.
    ///
    /// Setup: room at v0 with owner's blob only (the invitee was added
    /// after rotation kicked off, so they have no v0 blob yet). Rotate
    /// to v1 and assert the emitted set includes (member, 0) — back-fill
    /// — and (owner, 1) + (member, 1).
    #[test]
    fn ui_rotation_backfills_prior_versions_for_new_member() {
        use std::collections::BTreeSet;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let member_vk = member_sk.verifying_key();
        let member_id = MemberId::from(&member_vk);

        let mut room = make_private_owner_room(&owner_sk, &member_sk);

        // Seed state to look like a real post-create room: owner has a
        // v0 encrypted_secrets entry (because they sealed the room name
        // under it), but the just-added member does NOT — they joined
        // after v0 was created.
        let v0_secret = *room
            .secrets
            .get(&0)
            .expect("v0 secret seeded by make_private_owner_room");
        let (ct, n, ek) = encrypt_secret_for_member(&v0_secret, &owner_vk);
        let owner_v0_blob = AuthorizedEncryptedSecretForMember::new(
            EncryptedSecretForMemberV1 {
                member_id: owner_id,
                secret_version: 0,
                ciphertext: ct,
                nonce: n,
                sender_ephemeral_public_key: ek.to_bytes(),
                provider: owner_id,
            },
            &owner_sk,
        );
        room.room_state.secrets.encrypted_secrets = vec![owner_v0_blob];

        // Rotate via the UI fast-path.
        let delta = room
            .rotate_secret()
            .expect("rotation must succeed for private owner room");

        let emitted: BTreeSet<(MemberId, u32)> = delta
            .new_encrypted_secrets
            .iter()
            .map(|s| (s.secret.member_id, s.secret.secret_version))
            .collect();

        // owner@0 is already in state — must NOT re-emit.
        assert!(
            !emitted.contains(&(owner_id, 0)),
            "must not re-emit (owner, 0): contract would reject duplicate"
        );
        // The CRITICAL back-fill assertion: member gets v0 even though
        // we're rotating to v1.
        assert!(
            emitted.contains(&(member_id, 0)),
            "UI rotation must back-fill (member, 0); emitted: {:?}",
            emitted
        );
        // Both members get v1.
        assert!(
            emitted.contains(&(owner_id, 1)),
            "owner must get (owner, 1)"
        );
        assert!(
            emitted.contains(&(member_id, 1)),
            "member must get (member, 1)"
        );

        // The back-filled v0 blob must actually decrypt to the room's
        // real v0 secret (not a re-derived one). This is the
        // confidentiality-preservation half of the bug.
        let member_v0_blob = delta
            .new_encrypted_secrets
            .iter()
            .find(|s| s.secret.member_id == member_id && s.secret.secret_version == 0)
            .expect("member's v0 back-fill blob must be present");
        let recovered = decrypt_secret_from_member_blob_raw(
            &member_v0_blob.secret.ciphertext,
            &member_v0_blob.secret.nonce,
            &member_v0_blob.secret.sender_ephemeral_public_key,
            &member_sk,
        )
        .expect("member must be able to decrypt their back-fill blob");
        assert_eq!(
            recovered, v0_secret,
            "back-filled v0 must equal the actual v0 the room was sealed under"
        );
    }

    #[test]
    fn ui_rotation_bails_at_max_version() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);

        let mut room = make_private_owner_room(&owner_sk, &member_sk);
        room.room_state.secrets.current_version = u32::MAX;

        let res = room.rotate_secret();
        assert!(res.is_err());
        assert!(
            res.unwrap_err().contains("u32::MAX"),
            "rotation must refuse to wrap version"
        );
    }

    // ====================================================================
    // #251: repopulate_secrets_from_state must run on every network state
    // ingestion path, not just initial GET / load-rooms. Before #251 only
    // the initial-load paths repopulated `room_data.secrets`, so the
    // delegate's PR #245 back-fill of `encrypted_secrets` (which arrives
    // in a subsequent state update) never made it into the in-memory map
    // and the new member rendered everything as `[Encrypted message -
    // secret vN not available]` until they hard-refreshed.
    // ====================================================================

    /// Fixture: build a private room state from the INVITEE perspective —
    /// owner config, invitee is a member, version record exists for `v0`,
    /// but `encrypted_secrets` is empty (the bug's initial GET case).
    /// The local `secrets` HashMap is also empty.
    fn make_private_invitee_room(
        owner_sk: &SigningKey,
        invitee_sk: &SigningKey,
    ) -> ([u8; 32], RoomData) {
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();
        let owner_id: MemberId = owner_vk.into();

        let mut config = Configuration {
            owner_member_id: owner_id,
            privacy_mode: PrivacyMode::Private,
            ..Configuration::default()
        };
        config.configuration_version = 1;

        let mut room_state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, owner_sk),
            ..Default::default()
        };

        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: invitee_vk,
        };
        room_state
            .members
            .members
            .push(AuthorizedMember::new(member, owner_sk));

        let v0_secret =
            river_core::key_derivation::derive_room_secret(&owner_sk.to_bytes(), &owner_vk, 0);
        let v0_record = SecretVersionRecordV1 {
            version: 0,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: get_current_system_time(),
        };
        room_state
            .secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(v0_record, owner_sk));
        room_state.secrets.current_version = 0;
        // Deliberately leave encrypted_secrets empty — this is the
        // post-subscribe / pre-back-fill state.

        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes),
            &ContractCode::from(ROOM_CONTRACT_WASM),
        );

        let room = RoomData {
            owner_vk,
            room_state,
            self_sk: invitee_sk.clone(),
            contract_key,
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        };

        (v0_secret, room)
    }

    /// Push an encrypted-secret blob for `recipient_vk` at `version` into
    /// the room state. Models what the chat delegate's PR #245 back-fill
    /// does when a new member joins.
    fn append_encrypted_secret_for(
        room_state: &mut ChatRoomStateV1,
        owner_sk: &SigningKey,
        recipient_vk: &VerifyingKey,
        secret: &[u8; 32],
        version: u32,
    ) {
        let (ciphertext, nonce, ephemeral_pk) = encrypt_secret_for_member(secret, recipient_vk);
        let blob = EncryptedSecretForMemberV1 {
            member_id: MemberId::from(recipient_vk),
            secret_version: version,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral_pk.to_bytes(),
            provider: MemberId::from(&owner_sk.verifying_key()),
        };
        room_state
            .secrets
            .encrypted_secrets
            .push(AuthorizedEncryptedSecretForMember::new(blob, owner_sk));
    }

    /// Regression for #251: the timeline that produces the user-visible
    /// symptom. Initial GET hands the invitee a state with NO encrypted
    /// blob for them. Owner's delegate later back-fills the blob in a
    /// subsequent update. After applying the update,
    /// `repopulate_secrets_from_state` must decrypt the new blob so the
    /// renderer can read the room without the user having to hard-refresh.
    #[test]
    fn repopulate_secrets_after_delegate_backfill_picks_up_new_blob() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);

        // 1. Initial GET state: no blob for invitee yet. Helper is a
        //    no-op for decryption; current_secret_version still aligns
        //    with the contract's view.
        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(
            decrypted, 0,
            "no encrypted_secrets entries for invitee yet, so nothing to decrypt"
        );
        assert!(room.secrets.is_empty());
        assert_eq!(room.current_secret_version, Some(0));

        // 2. Owner's delegate runs the PR #245 back-fill and ships an
        //    update that adds the encrypted blob for the invitee.
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &invitee_sk.verifying_key(),
            &v0_secret,
            0,
        );

        // 3. The fix: subsequent state ingestion (apply_delta /
        //    update_room_state) must re-run the helper so the new blob
        //    gets decrypted.
        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(decrypted, 1, "the new back-filled blob must be decrypted");
        assert_eq!(
            room.secrets.get(&0u32),
            Some(&v0_secret),
            "decrypted plaintext must match the original room secret"
        );
        assert_eq!(room.current_secret_version, Some(0));

        // 4. Idempotency: calling repopulate again with no new blobs is
        //    a no-op (filtered out by the `contains_key` guard).
        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(
            decrypted, 0,
            "already-decrypted versions must not be re-decrypted"
        );
    }

    /// Regression for #295: the member-info self-heal must fire when the
    /// room secret arrives via a subscription UPDATE, not only via a GET.
    ///
    /// This reproduces the exact trigger condition the synchronizer's
    /// UPDATE paths (`apply_delta_inner` / `update_room_state_inner`) now
    /// act on: a private-room invitee whose accept-PUT omitted member_info
    /// (no secret to seal the nickname) is stranded as "Unknown". The owner's
    /// back-fill blob arrives in a later UPDATE; once
    /// `repopulate_secrets_from_state` decrypts it (`new_secrets > 0`),
    /// `build_member_info_heal` against the post-merge state must now return
    /// a `Some(Private-sealed)` entry to publish. Before the fix, that heal
    /// was only ever built on the GET path, so the member stayed "Unknown"
    /// for the rest of the session.
    #[test]
    fn member_info_heal_fires_when_secret_arrives_via_update() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        // The invitee chose a nickname at join time; the seal was deferred
        // because no secret was available, so it lives in `self_nickname`.
        room.self_nickname = Some("Invitee".to_string());

        // 1. Pre-back-fill: no secret blob yet. The secret repopulate is a
        //    no-op (`new_secrets == 0`) AND the heal must DEFER — the room is
        //    private and there is no secret to seal the nickname, so
        //    publishing now would leak plaintext. This is the stranded
        //    "Unknown" state.
        let new_secrets = room.repopulate_secrets_from_state();
        assert_eq!(new_secrets, 0, "no blob yet, nothing to decrypt");
        let pre_state = room.room_state.clone();
        assert!(
            room.build_member_info_heal(&pre_state).is_none(),
            "with no secret the heal must defer — member stays 'Unknown'"
        );

        // 2. The owner's delegate back-fills the encrypted secret blob; it
        //    arrives in a subscription UPDATE (the path that, before this
        //    fix, never ran the heal).
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &invitee_sk.verifying_key(),
            &v0_secret,
            0,
        );

        // 3. The synchronizer's UPDATE path repopulates secrets...
        let new_secrets = room.repopulate_secrets_from_state();
        assert_eq!(
            new_secrets, 1,
            "the back-filled blob must decrypt (this gates the heal trigger)"
        );

        // 4. ...and now, with the secret available, the heal MUST produce a
        //    self-signed, Private-sealed member_info so the invitee stops
        //    rendering as "Unknown" to every other peer. This is the
        //    behaviour the UPDATE-path wiring publishes.
        let post_state = room.room_state.clone();
        let heal = room
            .build_member_info_heal(&post_state)
            .expect("once the secret arrives the heal must fire on the UPDATE path");
        assert!(
            matches!(
                heal.member_info.preferred_nickname,
                SealedBytes::Private { .. }
            ),
            "private-room heal must seal the nickname, never publish plaintext"
        );
        heal.verify_signature_with_key(&invitee_sk.verifying_key())
            .expect("healed entry must be self-signed by the invitee");
        let nickname = crate::util::ecies::unseal_bytes(
            &heal.member_info.preferred_nickname,
            Some(&v0_secret),
        )
        .expect("sealed nickname must decrypt with the arrived secret");
        assert_eq!(
            nickname, b"Invitee",
            "the heal must seal the nickname the invitee chose at join time"
        );
    }

    /// Helper must skip blobs intended for other members — we can't
    /// decrypt them with our own signing key and shouldn't try.
    #[test]
    fn repopulate_secrets_skips_blobs_for_other_members() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let stranger_sk = SigningKey::generate(&mut rng);

        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        // Back-fill a blob, but for a different member.
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &stranger_sk.verifying_key(),
            &v0_secret,
            0,
        );

        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(decrypted, 0);
        assert!(room.secrets.is_empty());
    }

    /// Source-grep pin test: every state-ingestion path in the
    /// synchronizer AND response-handler must call
    /// `repopulate_secrets_from_state`. The helper is the load-bearing
    /// fix for #251 — if a future refactor drops the call from any
    /// path, the regression returns silently (the unit tests above only
    /// verify the helper's contract, not that its call sites still
    /// exist). See the bug-prevention-patterns guidance on source-grep
    /// pins in `~/code/freenet/.claude/rules/bug-prevention-patterns.md`.
    ///
    /// The match is a literal substring (`"repopulate_secrets_from_state("`)
    /// rather than a regex, so a comment elsewhere in the file that
    /// merely mentions the function name will inflate the count and
    /// fail the assertion — that's a deliberate fail-closed posture.
    /// If you legitimately want to reference the function in prose,
    /// avoid the trailing `(` (e.g. write
    /// "see `repopulate_secrets_from_state`" without parens).
    ///
    /// If you add a NEW state-ingestion path, update this test's
    /// expected count.
    #[test]
    fn repopulate_secrets_call_sites_pinned() {
        // (path-for-error-messages, expected_call_count, file_contents)
        let sites: &[(&str, usize, &str)] = &[
            (
                "ui/src/components/app/freenet_api/room_synchronizer.rs",
                2, // apply_delta_inner + update_room_state_inner
                include_str!("components/app/freenet_api/room_synchronizer.rs"),
            ),
            (
                "ui/src/components/app/freenet_api/response_handler.rs",
                1, // handle_load_rooms_response
                include_str!("components/app/freenet_api/response_handler.rs"),
            ),
            (
                "ui/src/components/app/freenet_api/response_handler/get_response.rs",
                // pending-invite branch + existing-room (replace) + existing-room (merge)
                // + backward-probe handler (replace) + backward-probe handler (merge).
                // The last two are the freenet/river#292 recovery path: when a
                // backward probe recovers stranded state from a legacy contract
                // generation, the recovered state is adopted into RoomData and
                // its private-room secrets must be repopulated, exactly like the
                // normal existing-room GET path.
                5,
                include_str!("components/app/freenet_api/response_handler/get_response.rs"),
            ),
        ];

        for (path, expected, contents) in sites {
            let actual = contents.matches("repopulate_secrets_from_state(").count();
            assert_eq!(
                actual, *expected,
                "expected {} call(s) to `repopulate_secrets_from_state` in {}, found {}. \
                 Removing this call regresses #251 — see RoomData::repopulate_secrets_from_state \
                 doc-comment.",
                expected, path, actual
            );
        }
    }

    /// Source-grep pin (freenet/river#295): the member-info self-heal must
    /// be wired into BOTH synchronizer UPDATE paths — `apply_delta_inner`
    /// (delta path) and `update_room_state_inner` (full-state path). The
    /// secret a private-room invitee needs to seal their nickname usually
    /// arrives via a subscription UPDATE, not a GET; the heal must fire there
    /// or the member stays "Unknown" for the rest of their session.
    ///
    /// The behavioural condition is unit-tested by
    /// `member_info_heal_fires_when_secret_arrives_via_update`, but that test
    /// exercises `build_member_info_heal` directly — it cannot prove the
    /// synchronizer actually CALLS it (the synchronizer paths depend on Dioxus
    /// signals that don't run in native tests). This grep pins the call sites
    /// so a refactor that drops one fails CI, mirroring the
    /// `repopulate_secrets_call_sites_pinned` pattern above.
    #[test]
    fn member_info_heal_update_path_wiring_pinned() {
        let synchronizer = include_str!("components/app/freenet_api/room_synchronizer.rs");
        let heal_calls = synchronizer
            .matches("build_member_info_heal(&room_data.room_state)")
            .count();
        assert_eq!(
            heal_calls, 2,
            "expected 2 `build_member_info_heal` call(s) in room_synchronizer.rs \
             (apply_delta_inner + update_room_state_inner), found {}. Removing one \
             regresses #295 — a private-room invitee whose secret arrives via UPDATE \
             stays 'Unknown' until the next GET.",
            heal_calls
        );
        let send_calls = synchronizer
            .matches("send_member_info_heal_update(")
            .count();
        // 1 fn definition + 2 call sites.
        assert_eq!(
            send_calls, 3,
            "expected the heal-UPDATE sender to be defined once and called from both \
             UPDATE paths in room_synchronizer.rs, found {} occurrence(s) of \
             `send_member_info_heal_update(`.",
            send_calls
        );
    }

    /// Helper must be a no-op on public rooms — there are no secrets to
    /// decrypt and the `secrets` map should stay empty.
    #[test]
    fn repopulate_secrets_is_noop_on_public_room() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let room_state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&params);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes),
            &ContractCode::from(ROOM_CONTRACT_WASM),
        );

        let mut room = RoomData {
            owner_vk,
            room_state,
            self_sk: member_sk,
            contract_key,
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
        };

        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(decrypted, 0);
        assert!(room.secrets.is_empty());
        assert_eq!(room.current_secret_version, None);
    }

    /// An invitee whose `encrypted_secrets` blob has not been back-filled
    /// yet must still get the room secret folded into the in-memory
    /// `secrets` map from `invitation_secrets` (carried in the
    /// invitation artifact) — this is what lets them read a private room
    /// without waiting for the owner delegate.
    #[test]
    fn repopulate_secrets_folds_invitation_secrets() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        // No encrypted_secrets blob — the secret arrived only via the
        // invitation artifact.
        room.invitation_secrets.insert(0, v0_secret);

        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(decrypted, 1, "invitation secret v0 must be folded in");
        assert_eq!(room.secrets.get(&0u32), Some(&v0_secret));
        assert_eq!(room.current_secret_version, Some(0));
    }

    /// When both the contract `encrypted_secrets` blob and an invitation
    /// secret exist for the same version, the contract blob is
    /// authoritative — it is decrypted first and the `contains_key`
    /// guard skips the invitation copy.
    #[test]
    fn repopulate_secrets_contract_blob_wins_over_invitation_secret() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &invitee_sk.verifying_key(),
            &v0_secret,
            0,
        );
        // A deliberately wrong invitation secret for the same version.
        room.invitation_secrets.insert(0, [0xABu8; 32]);

        room.repopulate_secrets_from_state();
        assert_eq!(
            room.secrets.get(&0u32),
            Some(&v0_secret),
            "the contract encrypted_secrets blob must win over invitation_secrets"
        );
    }

    /// `seal_invitee_nickname` falls back to the invitation-provided
    /// secret when the contract state carries no blob for this member.
    #[test]
    fn seal_invitee_nickname_uses_invitation_secret_as_fallback() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let (v0_secret, room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        // No blob -> the contract-state lookup yields nothing.
        assert!(current_secret_from_state(&room.room_state, &invitee_sk).is_none());

        let mut invitation_secrets = HashMap::new();
        invitation_secrets.insert(0u32, v0_secret);
        let sealed =
            seal_invitee_nickname(&room.room_state, &invitee_sk, &invitation_secrets, "Alice");
        assert!(
            sealed.is_some(),
            "invitation secret should let the nickname seal"
        );
    }

    /// `seal_invitee_nickname` returns `None` for a private room when no
    /// secret is available from either source — the caller defers
    /// member_info rather than leaking a plaintext nickname.
    #[test]
    fn seal_invitee_nickname_none_when_no_secret_available() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let (_v0_secret, room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        let sealed = seal_invitee_nickname(&room.room_state, &invitee_sk, &HashMap::new(), "Alice");
        assert!(sealed.is_none());
    }

    /// Regression for the cross-call ordering bug (PR #301 skeptical /
    /// Codex review): a stale or garbage invitation secret folded BEFORE
    /// the owner-signed `encrypted_secrets` blob arrives must NOT
    /// permanently shadow it. A later ingestion carrying the authentic
    /// blob has to overwrite the in-memory `secrets` value and prune the
    /// superseded entry from `invitation_secrets`.
    #[test]
    fn repopulate_secrets_contract_blob_overwrites_stale_invitation_secret() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);

        // Call 1: only a (garbage) invitation secret, no contract blob yet.
        room.invitation_secrets.insert(0, [0x11u8; 32]);
        room.repopulate_secrets_from_state();
        assert_eq!(
            room.secrets.get(&0u32),
            Some(&[0x11u8; 32]),
            "invitation secret is folded in while no contract blob exists"
        );

        // Call 2: the owner delegate back-fills the authentic blob.
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &invitee_sk.verifying_key(),
            &v0_secret,
            0,
        );
        room.repopulate_secrets_from_state();
        assert_eq!(
            room.secrets.get(&0u32),
            Some(&v0_secret),
            "the authentic contract blob must overwrite the stale invitation secret"
        );
        assert!(
            !room.invitation_secrets.contains_key(&0),
            "the superseded invitation secret must be pruned from invitation_secrets"
        );
    }

    /// Backward-compat: a `rooms_data` blob written before
    /// `invitation_secrets` (and the other `#[serde(default)]` fields)
    /// existed must still deserialize as a `RoomData`. Encodes a minimal
    /// struct carrying only the non-default fields.
    #[test]
    fn roomdata_decodes_from_minimal_legacy_blob() {
        #[derive(Serialize)]
        struct MinimalRoomData {
            owner_vk: VerifyingKey,
            room_state: ChatRoomStateV1,
            self_sk: SigningKey,
            contract_key: ContractKey,
        }
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let (_v0, room) = make_private_invitee_room(&owner_sk, &invitee_sk);

        let minimal = MinimalRoomData {
            owner_vk: room.owner_vk,
            room_state: room.room_state.clone(),
            self_sk: invitee_sk.clone(),
            contract_key: room.contract_key,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&minimal, &mut buf).unwrap();
        let decoded: RoomData =
            ciborium::de::from_reader(buf.as_slice()).expect("legacy RoomData blob must decode");
        assert!(decoded.invitation_secrets.is_empty());
        assert!(decoded.previous_contract_key.is_none());
    }

    /// Refresh durability: `invitation_secrets` is persisted while
    /// `secrets` is `#[serde(skip)]`, so after a serde round-trip
    /// (simulating a page reload) the in-memory map is empty but
    /// `repopulate_secrets_from_state` recovers it from the persisted
    /// `invitation_secrets`.
    #[test]
    fn invitation_secrets_survive_roomdata_serde_round_trip() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        room.invitation_secrets.insert(0, v0_secret);
        room.set_secret(v0_secret, 0);

        let mut buf = Vec::new();
        ciborium::ser::into_writer(&room, &mut buf).unwrap();
        let mut decoded: RoomData = ciborium::de::from_reader(buf.as_slice()).unwrap();

        assert_eq!(
            decoded.invitation_secrets.get(&0u32),
            Some(&v0_secret),
            "invitation_secrets must persist across the round-trip"
        );
        assert!(
            decoded.secrets.is_empty(),
            "secrets is #[serde(skip)] — empty after deserialize"
        );

        let recovered = decoded.repopulate_secrets_from_state();
        assert_eq!(
            recovered, 1,
            "the secret is recovered from invitation_secrets"
        );
        assert_eq!(decoded.secrets.get(&0u32), Some(&v0_secret));
    }

    /// `seal_invitee_nickname` defers (returns `None`) when the invitation
    /// carries no secret for the room's *current* version — e.g. the room
    /// rotated after the invitation was created.
    #[test]
    fn seal_invitee_nickname_none_when_invitation_lacks_current_version() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        // Room has rotated to v1; the invitation only carries v0.
        room.room_state.secrets.current_version = 1;
        let mut invitation_secrets = HashMap::new();
        invitation_secrets.insert(0u32, v0_secret);
        let sealed =
            seal_invitee_nickname(&room.room_state, &invitee_sk, &invitation_secrets, "Alice");
        assert!(
            sealed.is_none(),
            "no invitation secret at current_version → defer to self-heal"
        );
    }

    /// `seal_invitee_nickname` prefers the owner-signed contract secret
    /// over an invitation-carried secret for the same version.
    #[test]
    fn seal_invitee_nickname_prefers_contract_secret() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let (v0_secret, mut room) = make_private_invitee_room(&owner_sk, &invitee_sk);
        append_encrypted_secret_for(
            &mut room.room_state,
            &owner_sk,
            &invitee_sk.verifying_key(),
            &v0_secret,
            0,
        );
        // A wrong invitation secret for the same version.
        let mut invitation_secrets = HashMap::new();
        invitation_secrets.insert(0u32, [0xCDu8; 32]);
        let sealed =
            seal_invitee_nickname(&room.room_state, &invitee_sk, &invitation_secrets, "Bob")
                .expect("the contract secret should let the nickname seal");
        // Sealed against the contract secret → unsealing with it succeeds.
        let mut good = HashMap::new();
        good.insert(0u32, v0_secret);
        assert!(
            crate::util::ecies::unseal_bytes_with_secrets(&sealed, &good).is_ok(),
            "nickname must be sealed with the authoritative contract secret"
        );
    }

    /// Source-grep pin: `seal_invitee_nickname` must remain wired into the
    /// invitation-accept path. If a refactor drops the call and reverts to
    /// an inline `SealedBytes::public` seal, a private room would leak a
    /// plaintext nickname — this fails CI before that can happen.
    #[test]
    fn seal_invitee_nickname_call_site_pinned() {
        let contents = include_str!("components/app/freenet_api/response_handler/get_response.rs");
        assert_eq!(
            contents.matches("seal_invitee_nickname(").count(),
            1,
            "seal_invitee_nickname must be called exactly once in get_response.rs"
        );
    }
}
