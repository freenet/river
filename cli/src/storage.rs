use crate::api::compute_contract_key;
use anyhow::{anyhow, Result};
use directories::ProjectDirs;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::ContractKey;
use river_core::chat_delegate::OutboundDmStore;
use river_core::room_state::member::AuthorizedMember;
use river_core::room_state::ChatRoomStateV1;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRoomInfo {
    pub signing_key_bytes: [u8; 32],
    pub state: ChatRoomStateV1,
    pub contract_key: String, // Store as string for serialization
    /// The user's own AuthorizedMember, stored so they can re-add themselves
    /// after being pruned for inactivity (no recent messages).
    #[serde(default)]
    pub self_authorized_member: Option<AuthorizedMember>,
    /// The invite chain members needed to validate self_authorized_member.
    #[serde(default)]
    pub invite_chain: Vec<AuthorizedMember>,
    /// The previous contract key before WASM update, used for migration fallback.
    /// When the bundled WASM changes, this stores the old contract key so
    /// any client can GET state from the old contract and PUT it to the new one.
    #[serde(default)]
    pub previous_contract_key: Option<String>,
    /// Room secrets received via the `Invitation` artifact (issue freenet/river#302).
    /// Maps `secret_version` → 32-byte symmetric key. Empty for public rooms
    /// and for rooms joined before this field existed (`#[serde(default)]`
    /// keeps old `rooms.json` files readable).
    ///
    /// **Current consumers** (CLI):
    /// - `create_invitation` — folds these into `Invitation::room_secrets`
    ///   so the next invitee can decrypt the room immediately on join.
    /// - `accept_invitation` — seeds this map from a freshly-accepted
    ///   `Invitation::room_secrets` so the CLI persists the secret across
    ///   invocations.
    ///
    /// **Not yet consumed** for message/nickname decryption — the CLI has no
    /// private-room-decrypt path today, so this map is currently used only
    /// to forward secrets onward via new invitations. A CLI message-decrypt
    /// counterpart is the natural follow-up after freenet/river#304 (the
    /// CLI heal path); until then, do not assume reading from this map
    /// gates anything other than invitation forwarding.
    ///
    /// **Authority.** The owner-signed contract blob in
    /// `state.secrets.encrypted_secrets` is authoritative and supersedes an
    /// invitation-carried entry at the same version — mirrors the UI's
    /// `RoomData::invitation_secrets` semantics. NOTE: unlike the UI's
    /// `repopulate_secrets_from_state` (which prunes the
    /// invitation-carried entry once the owner blob arrives), the CLI does
    /// NOT prune. Storage waste only — see freenet/river#304 for the heal
    /// path that would naturally hook the prune.
    ///
    /// **Threat model.** Plaintext on disk, consistent with `signing_key_bytes`
    /// and the outbound-DM cache. Protected by filesystem permissions and
    /// whatever full-disk encryption the user has configured.
    #[serde(default)]
    pub invitation_secrets: HashMap<u32, [u8; 32]>,
    /// The member's own chosen nickname for this room, persisted so
    /// `ApiClient::build_rejoin_delta` can restore it (sealed for private
    /// rooms) when re-adding the member after an inactivity prune — instead
    /// of the generic "Member" placeholder. Set on `accept_invitation` and
    /// `set_nickname`. `None` for rooms joined before this field existed
    /// (`#[serde(default)]` keeps old `rooms.json` files readable) and for
    /// the room owner (who is never pruned, so never rejoins).
    #[serde(default)]
    pub self_nickname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoomStorage {
    /// Map from room owner verifying key (as base58) to room info
    pub rooms: HashMap<String, StoredRoomInfo>,
}

pub struct Storage {
    storage_path: PathBuf,
    /// Outbound-DM plaintext cache file (issue freenet/river#256).
    /// Side file so the larger `rooms.json` blob stays untouched on
    /// each DM send. JSON-serialized [`OutboundDmStore`].
    outbound_dms_path: PathBuf,
    /// In-memory signing-key override (from `--signing-key-file` flag or
    /// `RIVER_SIGNING_KEY_FILE` env var). When set, every call to
    /// [`Storage::get_room`] returns this key in place of the room's
    /// stored `signing_key_bytes`. Never written back to disk — the
    /// override is a per-command-invocation thing.
    ///
    /// Motivates: this machine often has multiple identities for the
    /// same room (room owner, invite bot, test alts), but `rooms.json`
    /// only stores ONE `signing_key_bytes` per room. The UI's chat-
    /// delegate sync can silently rewrite that field — leaving owner
    /// ops broken without a manual swap. The override lets owner ops
    /// nominate the right identity at command time without touching
    /// `rooms.json`. See discussion on river#281.
    signing_key_override: Option<SigningKey>,
}

impl Storage {
    pub fn new(config_dir: Option<&str>) -> Result<Self> {
        Self::new_with_override(config_dir, None)
    }

    /// Construct a [`Storage`] with an optional in-memory signing-key
    /// override. See the field doc on [`Storage::signing_key_override`].
    pub fn new_with_override(
        config_dir: Option<&str>,
        signing_key_override: Option<SigningKey>,
    ) -> Result<Self> {
        // Use provided config_dir, then check environment variable, then use default
        let data_dir = if let Some(dir) = config_dir {
            PathBuf::from(dir)
        } else if let Ok(config_dir) = std::env::var("RIVER_CONFIG_DIR") {
            PathBuf::from(config_dir)
        } else {
            // Fall back to default project directories
            let proj_dirs = ProjectDirs::from("", "Freenet", "River")
                .ok_or_else(|| anyhow!("Failed to determine project directories"))?;
            proj_dirs.data_dir().to_path_buf()
        };

        fs::create_dir_all(&data_dir)?;

        let storage_path = data_dir.join("rooms.json");
        let outbound_dms_path = data_dir.join("outbound_dms.json");

        Ok(Self {
            storage_path,
            outbound_dms_path,
            signing_key_override,
        })
    }

    /// Resolve the signing key to use for the current command: prefer
    /// the in-memory override if set, otherwise reconstruct from the
    /// per-room `signing_key_bytes`. Used by both [`Storage::get_room`]
    /// and [`crate::api::ApiClient::ensure_room_migrated`] (which has
    /// its own load_rooms snapshot for migration purposes).
    pub fn resolve_signing_key(&self, stored_bytes: &[u8; 32]) -> SigningKey {
        if let Some(override_key) = &self.signing_key_override {
            override_key.clone()
        } else {
            SigningKey::from_bytes(stored_bytes)
        }
    }

    /// Whether an in-memory signing-key override is active (from
    /// `--signing-key-file` / `RIVER_SIGNING_KEY_FILE`). Lets callers tailor
    /// diagnostics to the override-set vs not-set case without exposing the
    /// key itself. See the field doc on [`Storage::signing_key_override`].
    pub fn has_signing_key_override(&self) -> bool {
        self.signing_key_override.is_some()
    }

    pub fn load_rooms(&self) -> Result<RoomStorage> {
        if !self.storage_path.exists() {
            return Ok(RoomStorage::default());
        }

        let contents = fs::read_to_string(&self.storage_path)?;
        let mut storage: RoomStorage = serde_json::from_str(&contents)?;

        // Regenerate contract keys to ensure they match the current bundled WASM
        // This handles the case where rooms were stored with an older WASM version
        let mut updated = false;
        for (owner_key_str, room_info) in storage.rooms.iter_mut() {
            let owner_key_bytes = match bs58::decode(owner_key_str).into_vec() {
                Ok(bytes) if bytes.len() == 32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&bytes);
                    arr
                }
                _ => continue,
            };
            let owner_vk = match VerifyingKey::from_bytes(&owner_key_bytes) {
                Ok(vk) => vk,
                Err(_) => continue,
            };
            let new_key = compute_contract_key(&owner_vk);
            let new_key_str = new_key.id().to_string();
            if room_info.contract_key != new_key_str {
                info!(
                    "Updating contract key for room: {} -> {}",
                    room_info.contract_key, new_key_str
                );
                // Save old key for migration fallback before overwriting
                room_info.previous_contract_key = Some(room_info.contract_key.clone());
                room_info.contract_key = new_key_str;
                updated = true;
            }
        }

        // Save the updated storage if any keys changed
        if updated {
            self.save_rooms(&storage)?;
        }

        Ok(storage)
    }

    pub fn save_rooms(&self, storage: &RoomStorage) -> Result<()> {
        let contents = serde_json::to_string_pretty(storage)?;
        fs::write(&self.storage_path, contents)?;
        Ok(())
    }

    /// Load the local outbound-DM plaintext cache (issue
    /// freenet/river#256). Returns an empty store if the file does
    /// not exist; surfaces any other I/O or parse error.
    pub fn load_outbound_dms(&self) -> Result<OutboundDmStore> {
        if !self.outbound_dms_path.exists() {
            return Ok(OutboundDmStore::default());
        }
        let contents = fs::read_to_string(&self.outbound_dms_path)?;
        let store: OutboundDmStore = serde_json::from_str(&contents)?;
        Ok(store)
    }

    /// Persist the outbound-DM plaintext cache to disk.
    ///
    /// **Threat model note (#256 / #259 review).** This file is
    /// plaintext on disk — consistent with `rooms.json`, which also
    /// stores room signing keys and member state unencrypted. Both
    /// are protected by filesystem permissions on the user's data
    /// directory and by whatever full-disk encryption the user has
    /// configured. The UI path uses the chat delegate, whose secret
    /// store IS encrypted at rest; the CLI does NOT have an
    /// equivalent yet.
    pub fn save_outbound_dms(&self, store: &OutboundDmStore) -> Result<()> {
        let contents = serde_json::to_string_pretty(store)?;
        fs::write(&self.outbound_dms_path, contents)?;
        Ok(())
    }

    pub fn add_room(
        &self,
        owner_vk: &VerifyingKey,
        signing_key: &SigningKey,
        state: ChatRoomStateV1,
        contract_key: &ContractKey,
    ) -> Result<()> {
        self.add_room_with_invitation_secrets(
            owner_vk,
            signing_key,
            state,
            contract_key,
            HashMap::new(),
        )
    }

    /// Like [`Self::add_room`] but seeds the room's persisted
    /// `invitation_secrets` from an `Invitation` artifact (issue
    /// freenet/river#302). Used by `accept_invitation` so a CLI invitee can
    /// decrypt a private room on first read without waiting for the owner's
    /// chat-delegate to back-fill an `encrypted_secrets` blob.
    pub fn add_room_with_invitation_secrets(
        &self,
        owner_vk: &VerifyingKey,
        signing_key: &SigningKey,
        state: ChatRoomStateV1,
        contract_key: &ContractKey,
        invitation_secrets: HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        let mut storage = self.load_rooms()?;

        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        let room_info = StoredRoomInfo {
            signing_key_bytes: signing_key.to_bytes(),
            state,
            contract_key: contract_key.id().to_string(),
            self_authorized_member: None,
            invite_chain: Vec::new(),
            previous_contract_key: None,
            invitation_secrets,
            self_nickname: None,
        };

        storage.rooms.insert(owner_key_str, room_info);
        self.save_rooms(&storage)?;

        Ok(())
    }

    /// Persist the member's own nickname for `owner_vk`'s room, so a later
    /// rejoin (`ApiClient::build_rejoin_delta`) can restore it instead of the
    /// generic "Member" placeholder. No-op if the room isn't stored yet.
    pub fn update_self_nickname(&self, owner_vk: &VerifyingKey, nickname: &str) -> Result<()> {
        let mut storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        if let Some(info) = storage.rooms.get_mut(&owner_key_str) {
            info.self_nickname = Some(nickname.to_string());
            self.save_rooms(&storage)?;
        }
        Ok(())
    }

    /// Return the persisted invitation-carried secrets for a room, keyed by
    /// `secret_version`. Returns an empty map for rooms that predate
    /// freenet/river#302 (the `#[serde(default)]` keeps loading safe) and for
    /// public rooms, where no secrets are carried. Used by `create_invitation`
    /// to populate the outgoing invitation's `room_secrets`.
    pub fn get_invitation_secrets(
        &self,
        owner_vk: &VerifyingKey,
    ) -> Result<HashMap<u32, [u8; 32]>> {
        let storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        Ok(storage
            .rooms
            .get(&owner_key_str)
            .map(|r| r.invitation_secrets.clone())
            .unwrap_or_default())
    }

    pub fn get_room(
        &self,
        owner_vk: &VerifyingKey,
    ) -> Result<Option<(SigningKey, ChatRoomStateV1, String)>> {
        let storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

        if let Some(room_info) = storage.rooms.get(&owner_key_str) {
            let signing_key = self.resolve_signing_key(&room_info.signing_key_bytes);
            Ok(Some((
                signing_key,
                room_info.state.clone(),
                room_info.contract_key.clone(),
            )))
        } else {
            Ok(None)
        }
    }

    pub fn update_room_state(&self, owner_vk: &VerifyingKey, state: ChatRoomStateV1) -> Result<()> {
        let mut storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

        if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
            room_info.state = state;
            self.save_rooms(&storage)?;
            Ok(())
        } else {
            Err(anyhow!("Room not found"))
        }
    }

    /// Update the contract key for a room (used during migration to new contract version)
    pub fn update_contract_key(
        &self,
        owner_vk: &VerifyingKey,
        new_key: &ContractKey,
    ) -> Result<()> {
        let mut storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

        if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
            room_info.contract_key = new_key.id().to_string();
            self.save_rooms(&storage)?;
            Ok(())
        } else {
            Err(anyhow!("Room not found"))
        }
    }

    /// Forget all locally-stored credentials for a room: its
    /// `StoredRoomInfo` entry in `rooms.json` (signing key, state, membership,
    /// nickname, invitation secrets) AND any cached outbound-DM plaintext /
    /// archived-thread entries for that room in `outbound_dms.json`. Returns
    /// `true` if a room was removed, `false` if no room was stored for
    /// `owner_vk`.
    ///
    /// This is the deliberate-replace escape hatch for the re-accept guard
    /// (issue freenet/river#308): after `riverctl room leave <owner>` the
    /// `accept_invitation` guard no longer fires, so the user can accept a
    /// fresh invitation. It does NOT touch the network — the room contract and
    /// the user's on-chain membership are unaffected; this only drops the
    /// local client's copy.
    ///
    /// The outbound-DM cache is pruned too so leaving a room does not leave
    /// orphaned plaintext DM bodies on disk (Gemini review on PR #327). The
    /// prune is best-effort: a failure to rewrite `outbound_dms.json` does not
    /// fail the leave, since the authoritative `rooms.json` removal has already
    /// succeeded.
    pub fn remove_room(&self, owner_vk: &VerifyingKey) -> Result<bool> {
        let mut storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        let removed = storage.rooms.remove(&owner_key_str).is_some();
        if removed {
            self.save_rooms(&storage)?;
            if let Err(e) = self.prune_outbound_dms_for_room(owner_vk) {
                // Non-fatal: the room is already removed from rooms.json. Warn
                // so the orphaned plaintext is visible, but don't fail leave.
                tracing::warn!(
                    "room leave: failed to prune outbound-DM cache for {owner_key_str}: {e}"
                );
            }
        }
        Ok(removed)
    }

    /// Drop every cached outbound-DM plaintext entry and archived-thread entry
    /// for `owner_vk`'s room from `outbound_dms.json`. No-op if the cache file
    /// holds nothing for that room. Called by [`Self::remove_room`].
    fn prune_outbound_dms_for_room(&self, owner_vk: &VerifyingKey) -> Result<()> {
        let mut store = self.load_outbound_dms()?;
        let room_bytes = owner_vk.to_bytes();
        let before = store.entries.len() + store.hidden_threads.len();
        store.entries.retain(|e| e.room_owner_vk != room_bytes);
        store
            .hidden_threads
            .retain(|h| h.room_owner_vk != room_bytes);
        if store.entries.len() + store.hidden_threads.len() != before {
            self.save_outbound_dms(&store)?;
        }
        Ok(())
    }

    pub fn store_authorized_member(
        &self,
        owner_vk: &VerifyingKey,
        authorized_member: &AuthorizedMember,
        invite_chain: &[AuthorizedMember],
    ) -> Result<()> {
        let mut storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
            room_info.self_authorized_member = Some(authorized_member.clone());
            room_info.invite_chain = invite_chain.to_vec();
            self.save_rooms(&storage)?;
        }
        Ok(())
    }

    pub fn list_rooms(&self) -> Result<Vec<(VerifyingKey, String, String)>> {
        let storage = self.load_rooms()?;
        let mut rooms = Vec::new();

        for (owner_key_str, room_info) in storage.rooms.iter() {
            let owner_key_bytes = bs58::decode(owner_key_str).into_vec()?;
            if owner_key_bytes.len() == 32 {
                let mut key_array = [0u8; 32];
                key_array.copy_from_slice(&owner_key_bytes);
                if let Ok(owner_vk) = VerifyingKey::from_bytes(&key_array) {
                    let room_name = room_info
                        .state
                        .configuration
                        .configuration
                        .display
                        .name
                        .to_string_lossy();
                    rooms.push((owner_vk, room_name, room_info.contract_key.clone()));
                }
            }
        }

        Ok(rooms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use tempfile::TempDir;

    fn create_test_storage() -> (Storage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        (storage, temp_dir)
    }

    fn create_test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()))
    }

    /// Compute the expected contract key for a given owner verifying key.
    /// This matches what load_rooms will regenerate.
    fn expected_contract_key(owner_vk: &VerifyingKey) -> ContractKey {
        compute_contract_key(owner_vk)
    }

    fn create_test_state(owner_sk: &SigningKey) -> ChatRoomStateV1 {
        let owner_vk = owner_sk.verifying_key();
        let mut state = ChatRoomStateV1::default();
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            ..Default::default()
        };
        state.configuration = AuthorizedConfigurationV1::new(config, owner_sk);
        state
    }

    #[test]
    fn test_update_contract_key_success() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let initial_key = expected_contract_key(&owner_vk);

        // Add room with the computed contract key
        storage
            .add_room(&owner_vk, &owner_sk, state, &initial_key)
            .unwrap();

        // Verify the key is stored correctly (will be regenerated on load)
        let (_, _, stored_key) = storage.get_room(&owner_vk).unwrap().unwrap();
        assert_eq!(stored_key, initial_key.id().to_string());

        // Create a different key for testing update
        let different_key = {
            let code = freenet_stdlib::prelude::ContractCode::from(vec![42u8; 100]);
            let params = freenet_stdlib::prelude::Parameters::from(vec![42u8]);
            ContractKey::from_params_and_code(params, &code)
        };

        // Update to different key
        storage
            .update_contract_key(&owner_vk, &different_key)
            .unwrap();

        // After reload, key will be regenerated to match current WASM, not the updated key
        // This tests that update_contract_key persists, but load_rooms regenerates
        let (_, _, stored_key) = storage.get_room(&owner_vk).unwrap().unwrap();
        // The key gets regenerated on load, so it will be the expected key
        assert_eq!(stored_key, initial_key.id().to_string());
    }

    #[test]
    fn test_update_contract_key_room_not_found() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let new_key = expected_contract_key(&owner_vk);

        // Attempt to update non-existent room
        let result = storage.update_contract_key(&owner_vk, &new_key);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Room not found"));
    }

    #[test]
    fn test_update_self_nickname_round_trips() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let key = expected_contract_key(&owner_vk);
        storage.add_room(&owner_vk, &owner_sk, state, &key).unwrap();

        let key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        // Freshly-added rooms carry no nickname until one is set.
        assert_eq!(
            storage.load_rooms().unwrap().rooms[&key_str].self_nickname,
            None
        );

        storage.update_self_nickname(&owner_vk, "Alice").unwrap();
        assert_eq!(
            storage.load_rooms().unwrap().rooms[&key_str]
                .self_nickname
                .as_deref(),
            Some("Alice"),
            "nickname must persist across a load_rooms reload"
        );
    }

    /// Source-grep pins guarding the rejoin-nickname wiring against silent
    /// refactor regressions (testing-reviewer findings on PR #321). These live
    /// in storage.rs — NOT api.rs/identity.rs — so the pinned strings are not
    /// self-satisfied by the test's own source (the same discipline as
    /// `accept_invitation_calls_seal_invitee_nickname` in private_room.rs).
    #[test]
    fn rejoin_nickname_wiring_pinned() {
        let api_src = include_str!("api.rs");
        assert!(
            api_src.contains("rejoin_preferred_nickname("),
            "build_rejoin_delta must route the rejoin nickname through \
             `rejoin_preferred_nickname` (which seals for private rooms and \
             clamps to max_nickname_size). Do NOT inline an unconditional \
             public placeholder, or a private-room nickname could leak / an \
             over-long nickname could block rejoin."
        );
        assert!(
            api_src.contains("update_self_nickname(&room_owner_vk, nickname)"),
            "accept_invitation must persist the chosen nickname via \
             Storage::update_self_nickname."
        );
        assert!(
            api_src.contains("update_self_nickname(room_owner_key, &new_nickname)"),
            "set_nickname must persist the new nickname via \
             Storage::update_self_nickname."
        );
        let identity_src = include_str!("commands/identity.rs");
        assert!(
            identity_src.contains("update_self_nickname("),
            "import_identity must persist the imported (public-room) nickname \
             via Storage::update_self_nickname."
        );
    }

    /// Source-grep pins for the monitor edit/reply wiring (PR #322), in
    /// storage.rs so the pinned strings aren't self-satisfied by the scanned
    /// file (api.rs). Guards the exact regressions the PR fixed: a refactor
    /// reverting a monitor path to identity-only dedup (losing edits) or back to
    /// the colliding author:time key.
    #[test]
    fn monitor_edit_detection_wiring_pinned() {
        let api_src = include_str!("api.rs");
        // Both monitor paths (polling + subscribe) must route through the shared
        // scan helper so edit detection applies to both.
        let routed = api_src.matches("Self::emit_new_and_edited(").count();
        assert!(
            routed >= 2,
            "both monitor paths must call emit_new_and_edited (found {routed}); \
             do not inline identity-only dedup in either loop or edits stop \
             surfacing again"
        );
        // The dedup key must be the stable signature-derived id, not author:time.
        assert!(
            api_src.contains("monitor_seen_key(msg)"),
            "monitor dedup must key on monitor_seen_key (msg.id()); keying on \
             author:time lets a same-author same-timestamp collision flip-flop \
             as a spurious edit"
        );
        // Both monitor paths must also surface deletions (#323).
        let deletes = api_src.matches("Self::emit_deletions(").count();
        assert!(
            deletes >= 2,
            "both monitor paths must call emit_deletions so deletions surface \
             as events (found {deletes})"
        );
        // Both monitor paths must also surface live reaction changes (#325).
        let reactions = api_src.matches("Self::emit_reaction_changes(").count();
        assert!(
            reactions >= 2,
            "both monitor paths must call emit_reaction_changes so reactions \
             added/removed after a message was streamed surface as events \
             (found {reactions})"
        );
    }

    #[test]
    fn test_update_self_nickname_missing_room_is_noop() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        // No room stored for this key — must be a no-op, not an error.
        storage
            .update_self_nickname(&owner_sk.verifying_key(), "Ghost")
            .expect("updating a nickname for an unknown room should be a no-op");
    }

    #[test]
    fn test_stored_room_info_without_self_nickname_field_defaults_none() {
        // An old `rooms.json` written before the `self_nickname` field existed
        // must still deserialize, with `self_nickname` defaulting to `None`
        // (the `#[serde(default)]` backward-compat invariant).
        let owner_sk = create_test_signing_key();
        let info = StoredRoomInfo {
            signing_key_bytes: owner_sk.to_bytes(),
            state: create_test_state(&owner_sk),
            contract_key: "test".to_string(),
            self_authorized_member: None,
            invite_chain: Vec::new(),
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
            self_nickname: Some("Alice".to_string()),
        };
        let mut value = serde_json::to_value(&info).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("self_nickname")
            .expect("serialized form should contain self_nickname");
        let parsed: StoredRoomInfo = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.self_nickname, None);
    }

    #[test]
    fn test_update_contract_key_preserves_state() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let initial_key = expected_contract_key(&owner_vk);

        // Add room
        storage
            .add_room(&owner_vk, &owner_sk, state.clone(), &initial_key)
            .unwrap();

        // Create a different key for testing update
        let different_key = {
            let code = freenet_stdlib::prelude::ContractCode::from(vec![99u8; 100]);
            let params = freenet_stdlib::prelude::Parameters::from(vec![99u8]);
            ContractKey::from_params_and_code(params, &code)
        };

        // Update contract key
        storage
            .update_contract_key(&owner_vk, &different_key)
            .unwrap();

        // Verify state is preserved (key will be regenerated but state should remain)
        let (retrieved_sk, retrieved_state, _) = storage.get_room(&owner_vk).unwrap().unwrap();
        assert_eq!(retrieved_sk.to_bytes(), owner_sk.to_bytes());
        assert_eq!(
            retrieved_state.configuration.configuration.max_members,
            state.configuration.configuration.max_members
        );
    }

    #[test]
    fn test_load_rooms_sets_previous_contract_key_on_mismatch() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);

        // Store with a fake old contract key (simulating a WASM change)
        let fake_old_key = {
            let code = freenet_stdlib::prelude::ContractCode::from(vec![1u8; 100]);
            let params = freenet_stdlib::prelude::Parameters::from(vec![1u8]);
            ContractKey::from_params_and_code(params, &code)
        };
        storage
            .add_room(&owner_vk, &owner_sk, state, &fake_old_key)
            .unwrap();

        // Verify initial state: add_room stores the fake_old_key verbatim (the file
        // doesn't exist yet when load_rooms runs inside add_room, so no regeneration).
        let raw_storage: RoomStorage =
            serde_json::from_str(&std::fs::read_to_string(&storage.storage_path).unwrap()).unwrap();
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        assert_eq!(
            raw_storage.rooms[&owner_key_str].previous_contract_key,
            None
        );

        // Now load_rooms should detect the mismatch and set previous_contract_key
        let loaded = storage.load_rooms().unwrap();
        let room_info = loaded.rooms.get(&owner_key_str).unwrap();

        // The contract key should now be the current WASM-derived key
        let expected = expected_contract_key(&owner_vk);
        assert_eq!(room_info.contract_key, expected.id().to_string());

        // previous_contract_key should be set to the fake old key
        assert_eq!(
            room_info.previous_contract_key.as_deref(),
            Some(fake_old_key.id().to_string().as_str())
        );

        // Verify the update was persisted to disk
        let persisted: RoomStorage =
            serde_json::from_str(&std::fs::read_to_string(&storage.storage_path).unwrap()).unwrap();
        assert_eq!(
            persisted.rooms[&owner_key_str]
                .previous_contract_key
                .as_deref(),
            Some(fake_old_key.id().to_string().as_str())
        );
    }

    #[test]
    fn test_storage_roundtrip() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let contract_key = expected_contract_key(&owner_vk);

        // Add room
        storage
            .add_room(&owner_vk, &owner_sk, state.clone(), &contract_key)
            .unwrap();

        // Retrieve and verify
        let (retrieved_sk, retrieved_state, retrieved_key) =
            storage.get_room(&owner_vk).unwrap().unwrap();

        assert_eq!(retrieved_sk.to_bytes(), owner_sk.to_bytes());
        // The contract key should match the expected key (computed from owner_vk + current WASM)
        assert_eq!(retrieved_key, contract_key.id().to_string());
        assert_eq!(
            retrieved_state.configuration.configuration.max_members,
            state.configuration.configuration.max_members
        );

        // When WASM hasn't changed, previous_contract_key must be None
        // (ensures ensure_room_migrated returns early without network calls)
        let loaded = storage.load_rooms().unwrap();
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        assert_eq!(
            loaded.rooms[&owner_key_str].previous_contract_key, None,
            "previous_contract_key should be None when WASM hasn't changed"
        );
    }

    /// `--signing-key-file` / `RIVER_SIGNING_KEY_FILE` override: `Storage::get_room`
    /// must return the override key in place of the room's stored
    /// `signing_key_bytes`, AND the override must NOT be written back to
    /// `rooms.json`. This pins the "in-memory only" contract documented
    /// on `Storage::signing_key_override` — the persistent on-disk
    /// identity must be untouched, so subsequent invocations without
    /// the override revert to the stored identity.
    #[test]
    fn signing_key_override_is_returned_and_not_persisted() {
        let temp_dir = TempDir::new().unwrap();
        let stored_sk = create_test_signing_key();
        let override_sk = create_test_signing_key();
        assert_ne!(
            stored_sk.to_bytes(),
            override_sk.to_bytes(),
            "test invariant: stored and override keys must differ"
        );
        let owner_vk = stored_sk.verifying_key();

        // Set up storage with the stored identity (no override).
        let storage_no_override = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        let state = create_test_state(&stored_sk);
        let initial_key = expected_contract_key(&owner_vk);
        storage_no_override
            .add_room(&owner_vk, &stored_sk, state, &initial_key)
            .unwrap();

        // Sanity: without override, get_room returns the stored key.
        let (sk_no_override, _, _) = storage_no_override
            .get_room(&owner_vk)
            .unwrap()
            .expect("room present");
        assert_eq!(
            sk_no_override.to_bytes(),
            stored_sk.to_bytes(),
            "no override → stored key"
        );

        // With override, get_room returns the override.
        let storage_with_override = Storage::new_with_override(
            Some(temp_dir.path().to_str().unwrap()),
            Some(override_sk.clone()),
        )
        .unwrap();
        let (sk_with_override, _, _) = storage_with_override
            .get_room(&owner_vk)
            .unwrap()
            .expect("room present");
        assert_eq!(
            sk_with_override.to_bytes(),
            override_sk.to_bytes(),
            "override → override key returned"
        );

        // Critical: rooms.json on disk is untouched. A fresh Storage
        // without the override must see the ORIGINAL stored bytes.
        let storage_fresh = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        let (sk_fresh, _, _) = storage_fresh
            .get_room(&owner_vk)
            .unwrap()
            .expect("room present");
        assert_eq!(
            sk_fresh.to_bytes(),
            stored_sk.to_bytes(),
            "override must NOT be written back to rooms.json"
        );
    }

    /// `add_room_with_invitation_secrets` + `get_invitation_secrets` round
    /// trips the persisted map through disk. Pinning this here means a
    /// future serde-attr change on `StoredRoomInfo` cannot silently break
    /// invitation-secret persistence. (Issue freenet/river#302 PR #303
    /// testing-reviewer finding #6.)
    #[test]
    fn invitation_secrets_round_trip_via_disk() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let key = expected_contract_key(&owner_vk);

        let mut invitation_secrets: HashMap<u32, [u8; 32]> = HashMap::new();
        invitation_secrets.insert(0, [0x11u8; 32]);
        invitation_secrets.insert(1, [0x22u8; 32]);
        storage
            .add_room_with_invitation_secrets(
                &owner_vk,
                &owner_sk,
                state,
                &key,
                invitation_secrets.clone(),
            )
            .unwrap();

        // Reload through a FRESH Storage so the assertion exercises the
        // disk path, not an in-memory carry-over.
        let storage_fresh = Storage::new(Some(_temp_dir.path().to_str().unwrap())).unwrap();
        let retrieved = storage_fresh.get_invitation_secrets(&owner_vk).unwrap();
        assert_eq!(
            retrieved, invitation_secrets,
            "invitation_secrets must survive disk round-trip byte-for-byte"
        );
    }

    /// Pre-#302 `rooms.json` shape: no `invitation_secrets` key. The
    /// `#[serde(default)]` attribute MUST keep loading clean, with the
    /// new field defaulting to an empty map. Mirrors the UI's
    /// `roomdata_decodes_from_minimal_legacy_blob`. (Testing-reviewer
    /// finding #5.)
    #[test]
    fn rooms_json_decodes_legacy_blob_without_invitation_secrets_field() {
        let temp_dir = TempDir::new().unwrap();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        let key = expected_contract_key(&owner_vk);

        // Hand-build a JSON blob in the PRE-#302 shape — six fields, no
        // `invitation_secrets` key. Anything else would mean we're only
        // testing the current serialization round-tripping, not legacy
        // forward-compat.
        let legacy_blob = serde_json::json!({
            "rooms": {
                &owner_key_str: {
                    "signing_key_bytes": owner_sk.to_bytes().to_vec(),
                    "state": state,
                    "contract_key": key.id().to_string(),
                    "self_authorized_member": null,
                    "invite_chain": [],
                    "previous_contract_key": null,
                }
            }
        });
        let storage_path = temp_dir.path().join("rooms.json");
        std::fs::write(&storage_path, legacy_blob.to_string()).unwrap();

        let storage = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        let loaded = storage.load_rooms().unwrap();
        let room = loaded
            .rooms
            .get(&owner_key_str)
            .expect("legacy rooms.json must still load");
        assert!(
            room.invitation_secrets.is_empty(),
            "legacy rooms.json with no `invitation_secrets` field must deserialize \
             with an empty map (the `#[serde(default)]` invariant)"
        );
        // Sanity: get_invitation_secrets agrees.
        assert!(storage
            .get_invitation_secrets(&owner_vk)
            .unwrap()
            .is_empty());
    }

    /// Regression for issue freenet/river#308.
    ///
    /// `accept_invitation` used to call `add_room_with_invitation_secrets`
    /// unconditionally, which rebuilds the `StoredRoomInfo` and `insert`s it,
    /// wholesale-clobbering an existing room's `signing_key_bytes`,
    /// `self_authorized_member`, `invite_chain`, and `self_nickname`. This
    /// test pins BOTH halves of the fix:
    ///
    /// 1. The destructive behavior is real — a second
    ///    `add_room_with_invitation_secrets` for the same owner wipes the
    ///    previously-stored fields. This is exactly what the #308 guard must
    ///    prevent; if this assertion ever stops holding, the guard's reason to
    ///    exist has changed and the test should be revisited.
    /// 2. `get_room(...).is_some()` — the condition the guard checks in
    ///    `accept_invitation` — is true after the first accept, so the guard
    ///    fires on the re-accept path.
    #[test]
    fn reaccept_guard_prevents_clobber() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let key = expected_contract_key(&owner_vk);
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

        // First accept: store the room under the invitee's identity, plus the
        // ancillary fields a real accept populates (authorized member, invite
        // chain, nickname).
        let first_identity = create_test_signing_key();
        storage
            .add_room(&owner_vk, &first_identity, state.clone(), &key)
            .unwrap();
        let authorized_member = AuthorizedMember::new(
            river_core::room_state::member::Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: first_identity.verifying_key(),
            },
            &owner_sk,
        );
        storage
            .store_authorized_member(
                &owner_vk,
                &authorized_member,
                std::slice::from_ref(&authorized_member),
            )
            .unwrap();
        storage.update_self_nickname(&owner_vk, "Alice").unwrap();

        // Sanity: the guard's trigger condition is now true.
        assert!(
            storage.get_room(&owner_vk).unwrap().is_some(),
            "after first accept the room exists, so accept_invitation's \
             `get_room(...).is_some()` guard must fire on re-accept"
        );

        // Snapshot the populated fields.
        let before = storage.load_rooms().unwrap().rooms[&owner_key_str].clone();
        assert_eq!(before.signing_key_bytes, first_identity.to_bytes());
        assert!(before.self_authorized_member.is_some());
        assert!(!before.invite_chain.is_empty());
        assert_eq!(before.self_nickname.as_deref(), Some("Alice"));

        // Re-accept with a DIFFERENT identity, calling the same storage path
        // `accept_invitation` would have called pre-fix. Without the guard
        // this silently clobbers the stored identity and ancillary fields.
        let second_identity = create_test_signing_key();
        assert_ne!(first_identity.to_bytes(), second_identity.to_bytes());
        storage
            .add_room_with_invitation_secrets(
                &owner_vk,
                &second_identity,
                state,
                &key,
                HashMap::new(),
            )
            .unwrap();

        let after = storage.load_rooms().unwrap().rooms[&owner_key_str].clone();
        // These assertions DOCUMENT the destructive behavior the guard exists
        // to prevent: the storage primitive is intentionally a full replace.
        assert_eq!(
            after.signing_key_bytes,
            second_identity.to_bytes(),
            "add_room_with_invitation_secrets is a full replace — the stored \
             identity flips. The #308 guard in accept_invitation is what stops \
             this from happening silently on re-accept."
        );
        assert!(
            after.self_authorized_member.is_none(),
            "self_authorized_member is wiped by the full replace"
        );
        assert!(
            after.invite_chain.is_empty(),
            "invite_chain is wiped by the full replace"
        );
        assert_eq!(
            after.self_nickname, None,
            "self_nickname is wiped by the full replace"
        );
    }

    /// `remove_room` is the recovery path the #308 re-accept guard points
    /// users at (`riverctl room leave <owner>`). It must actually drop the
    /// stored entry so a subsequent `accept_invitation` no longer trips the
    /// guard — otherwise the guard would be a dead end (Codex review P2 on
    /// PR #327). Returns `true` only when something was removed.
    #[test]
    fn remove_room_clears_stored_entry() {
        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let state = create_test_state(&owner_sk);
        let key = expected_contract_key(&owner_vk);
        let identity = create_test_signing_key();

        // Removing a room that was never stored is a no-op returning false.
        assert!(
            !storage.remove_room(&owner_vk).unwrap(),
            "removing a non-existent room must return false"
        );

        storage.add_room(&owner_vk, &identity, state, &key).unwrap();
        assert!(storage.get_room(&owner_vk).unwrap().is_some());

        // Removing the stored room returns true and clears it, so the #308
        // guard (`get_room(...).is_some()`) no longer fires.
        assert!(
            storage.remove_room(&owner_vk).unwrap(),
            "removing a stored room must return true"
        );
        assert!(
            storage.get_room(&owner_vk).unwrap().is_none(),
            "after `room leave` the guard's `get_room(...).is_some()` must be false \
             so a fresh accept succeeds"
        );
    }

    /// Leaving a room must also drop that room's cached outbound-DM plaintext
    /// and archived-thread entries from `outbound_dms.json`, so leaving does
    /// not leave orphaned plaintext on disk (Gemini review on PR #327). Other
    /// rooms' DM entries must survive.
    #[test]
    fn remove_room_prunes_outbound_dm_cache() {
        use freenet_scaffold::util::FastHash;
        use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmEntry, OutboundDmStore};
        use river_core::room_state::direct_messages::PurgeToken;
        use river_core::room_state::member::MemberId;

        let (storage, _temp_dir) = create_test_storage();
        let left_sk = create_test_signing_key();
        let left_vk = left_sk.verifying_key();
        let kept_sk = create_test_signing_key();
        let kept_vk = kept_sk.verifying_key();

        // Store the room we will leave.
        let state = create_test_state(&left_sk);
        let key = expected_contract_key(&left_vk);
        storage
            .add_room(&left_vk, &create_test_signing_key(), state, &key)
            .unwrap();

        // Seed the outbound-DM cache with entries for BOTH rooms.
        let peer = MemberId(FastHash(42));
        let store = OutboundDmStore {
            entries: vec![
                OutboundDmEntry {
                    room_owner_vk: left_vk.to_bytes(),
                    sender: peer,
                    recipient: peer,
                    purge_token: PurgeToken([1u8; 16]),
                    timestamp: 1,
                    plaintext: "left-room secret".to_string(),
                },
                OutboundDmEntry {
                    room_owner_vk: kept_vk.to_bytes(),
                    sender: peer,
                    recipient: peer,
                    purge_token: PurgeToken([2u8; 16]),
                    timestamp: 2,
                    plaintext: "kept-room secret".to_string(),
                },
            ],
            hidden_threads: vec![
                HiddenDmThreadEntry {
                    room_owner_vk: left_vk.to_bytes(),
                    peer,
                    hidden_at_ts: 1,
                },
                HiddenDmThreadEntry {
                    room_owner_vk: kept_vk.to_bytes(),
                    peer,
                    hidden_at_ts: 2,
                },
            ],
        };
        storage.save_outbound_dms(&store).unwrap();

        assert!(storage.remove_room(&left_vk).unwrap());

        let after = storage.load_outbound_dms().unwrap();
        assert_eq!(
            after.entries.len(),
            1,
            "the left room's outbound-DM plaintext must be pruned"
        );
        assert_eq!(after.entries[0].room_owner_vk, kept_vk.to_bytes());
        assert_eq!(
            after.hidden_threads.len(),
            1,
            "the left room's archived-thread entry must be pruned"
        );
        assert_eq!(after.hidden_threads[0].room_owner_vk, kept_vk.to_bytes());
    }
}
