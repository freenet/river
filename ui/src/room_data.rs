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
    /// The plaintext nickname the local user chose when joining this room.
    ///
    /// Retained so the member-info self-heal ([`RoomData::build_member_info_heal`])
    /// can restore the user's *chosen* nickname rather than a generated
    /// default handle. It is needed because `self_member_info` cannot always
    /// be built at join time: a private room whose secret has not yet
    /// arrived can't seal the nickname, so the member_info is deferred to
    /// the heal — and by then the join-time `PendingRoomJoin` (the only
    /// other place the choice was recorded) has been discarded. `None` for
    /// the owner, for rooms joined before this field existed, and for
    /// imported rooms.
    #[serde(default)]
    pub self_nickname: Option<String>,
    /// The previous contract key before WASM update, used for migration fallback.
    /// When the bundled WASM changes, this stores the old contract key so
    /// any client can GET state from the old contract and PUT it to the new one.
    #[serde(default)]
    pub previous_contract_key: Option<ContractKey>,
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
    /// Returns the number of new versions decrypted (for logging).
    pub fn repopulate_secrets_from_state(&mut self) -> usize {
        use dioxus::logger::tracing::warn;

        if !self.is_private() {
            return 0;
        }

        // (secret_version, ciphertext, nonce, sender_ephemeral_x25519_pk_bytes)
        type PendingBlob = (u32, Vec<u8>, [u8; 12], [u8; 32]);

        let member_id = MemberId::from(&self.self_sk.verifying_key());

        // Snapshot the encrypted blobs we don't yet have plaintext for,
        // so we can release the borrow on `room_state` before calling
        // `set_secret` (which holds `&mut self`).
        let pending: Vec<PendingBlob> = self
            .room_state
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|s| s.secret.member_id == member_id)
            .filter(|s| !self.secrets.contains_key(&s.secret.secret_version))
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
                    self.set_secret(secret, version);
                    decrypted_count += 1;
                }
                Err(e) => {
                    warn!(
                        "repopulate_secrets_from_state: failed to decrypt v{} for member {:?}: {}",
                        version, member_id, e
                    );
                }
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

        // Always update self_member_info to latest version
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

    /// Build the members + member_info deltas needed to re-add ourselves to the room
    /// after being pruned for inactivity. Returns (None, None) if we're already a member
    /// or don't have stored credentials to re-add.
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

        // Use stored member_info to preserve nickname, or fall back to "Member"
        let authorized_info = if let Some(ref stored_info) = self.self_member_info {
            stored_info.clone()
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
            let member_info = MemberInfo {
                member_id,
                version: existing_version,
                preferred_nickname: SealedBytes::public("Member".to_string().into_bytes()),
            };
            AuthorizedMemberInfo::new_with_member_key(member_info, &self.self_sk)
        };

        (
            Some(river_core::room_state::member::MembersDelta::new(
                members_to_add,
            )),
            Some(vec![authorized_info]),
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
    /// Rooms whose contract key changed due to WASM update.
    /// Each entry is (owner_vk, old_contract_key) for rooms where the owner
    /// should send an upgrade pointer to the old contract.
    #[serde(skip)]
    pub migrated_rooms: Vec<(VerifyingKey, ContractKey)>,
}

impl PartialEq for Rooms {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map && self.removed_rooms == other.removed_rooms
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
        }
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
        }
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
            }
        };

        let mut current = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
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
            }
        };

        let mut current = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
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
            }
        };

        // Receiver has the room in `map`. Sender has it in `removed_rooms`.
        // After merge, neither side has it.
        let mut receiver = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
            migrated_rooms: Vec::new(),
        };
        receiver.map.insert(owner_vk, room_data);
        let mut sender = Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: std::collections::HashSet::new(),
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
        };

        let decrypted = room.repopulate_secrets_from_state();
        assert_eq!(decrypted, 0);
        assert!(room.secrets.is_empty());
        assert_eq!(room.current_secret_version, None);
    }
}
