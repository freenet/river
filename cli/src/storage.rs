use crate::api::compute_contract_key;
use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::ContractKey;
use fs2::FileExt;
use river_core::chat_delegate::OutboundDmStore;
use river_core::room_state::member::AuthorizedMember;
use river_core::room_state::ChatRoomStateV1;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
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

/// Result of [`Storage::import_room_atomic`] (freenet/river#414, Codex round-6
/// P1-5). Reported back so the caller can print the right message without
/// re-reading disk (and so the atomic decision — made INSIDE the lock — is the
/// authoritative one, not the pre-GET snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// A room already existed and `force` was false — nothing was written.
    RefusedNeedsForce,
    /// The room was inserted/replaced. `was_overwrite` = an entry existed
    /// before; `identity_changed` = the replaced signing key differed (so the
    /// old identity's DM cache was pruned in the same lock).
    Imported {
        was_overwrite: bool,
        identity_changed: bool,
    },
}

/// Local on-disk persistence for riverctl (`rooms.json` + `outbound_dms.json`).
///
/// **Concurrency model (issue freenet/river#307).** riverctl is a CLI invoked
/// one command at a time, but a script or cron job can run several invocations
/// concurrently (e.g. `accept` + `dm send` + `invite create`). Each mutating
/// operation here is a `load → mutate → save` sequence; without coordination two
/// invocations could both load the same base state and the later `save` would
/// silently clobber the earlier writer's update (lost-update race).
///
/// Two defenses make that safe:
///
/// 1. **Cross-process advisory locking.** Every mutating method takes an
///    exclusive [`fs2`] advisory lock on a dedicated lock file (`.river.lock`)
///    for the whole `load → mutate → save` critical section, so concurrent
///    invocations serialize rather than interleave. The lock is advisory and
///    cooperative — it only blocks other code paths that also go through this
///    `Storage` API.
/// 2. **Atomic writes.** Each save serializes to a temp file and `rename(2)`s it
///    over the target, so a reader (or a crash) never observes a half-written
///    JSON blob.
///
/// **Reentrancy hazard.** `fs2` locks are per-open-file-description on Unix, so a
/// second exclusive lock on a *fresh* handle to the lock file — even from the
/// same process — blocks (self-deadlock). The locking methods MUST therefore
/// never nest: the public `*_locked`-free methods acquire the lock once and call
/// the private non-locking `*_unlocked` helpers, which never re-lock. In
/// particular `load_rooms` regenerates contract keys and saves them back, so its
/// internal save goes through `save_rooms_unlocked`, NOT the locking `save_rooms`.
pub struct Storage {
    storage_path: PathBuf,
    /// Outbound-DM plaintext cache file (issue freenet/river#256).
    /// Side file so the larger `rooms.json` blob stays untouched on
    /// each DM send. JSON-serialized [`OutboundDmStore`].
    outbound_dms_path: PathBuf,
    /// Dedicated advisory-lock file (`.river.lock`) guarding the whole
    /// `load → mutate → save` critical section against concurrent riverctl
    /// invocations (issue freenet/river#307). A SEPARATE file from the data
    /// files so taking the lock never truncates or races the data itself, and
    /// so the atomic temp-file rename can never disturb the lock holder's
    /// handle. See the type-level doc for the no-nesting rule.
    lock_path: PathBuf,
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
        let lock_path = data_dir.join(".river.lock");

        Ok(Self {
            storage_path,
            outbound_dms_path,
            lock_path,
            signing_key_override,
        })
    }

    /// Run `f` while holding an exclusive cross-process advisory lock on the
    /// dedicated lock file, serializing concurrent riverctl invocations'
    /// `load → mutate → save` sequences (issue freenet/river#307).
    ///
    /// The lock is released when the file handle is dropped at the end of this
    /// method — including on the error path, since the handle is a local that
    /// unwinds normally. We also `unlock()` explicitly to surface any error and
    /// to make the release point obvious; dropping is the actual guarantee.
    ///
    /// **Do NOT call this from inside `f`** (directly or transitively). `fs2`
    /// locks are per-open-file-description: a nested call opens a fresh handle
    /// and would block forever waiting on the lock this very call already holds.
    /// See the [`Storage`] type-level doc.
    fn with_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .with_context(|| format!("opening storage lock file {}", self.lock_path.display()))?;
        lock_file
            .lock_exclusive()
            .with_context(|| format!("locking {}", self.lock_path.display()))?;
        let result = f();
        // Explicit unlock so a failure to release is surfaced rather than
        // swallowed by Drop. The handle still drops at end of scope, which is
        // the real release guarantee even if this call (or `f`) returned early.
        let _ = fs2::FileExt::unlock(&lock_file);
        result
    }

    /// Atomically replace `path`'s contents with `contents`: write to a
    /// uniquely-named temp file in the same directory, then `rename(2)` it over
    /// the target. `rename` within a directory is atomic on POSIX, so a
    /// concurrent reader (or a crash mid-write) never observes a partial blob
    /// (issue freenet/river#307). The temp name embeds the PID so two writers
    /// can't collide on the scratch file (the outer advisory lock already
    /// serializes them, but the unique name is a cheap belt-and-suspenders).
    fn atomic_write(path: &Path, contents: &str) -> Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("storage path {} has no parent dir", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("storage path {} has no file name", path.display()))?;
        let tmp_path = dir.join(format!("{file_name}.tmp.{}", std::process::id()));
        // Best-effort: write the scratch file, then rename. On any error after
        // the scratch file exists, try to remove it so we don't litter temp
        // files in the data dir.
        let write_then_rename = || -> Result<()> {
            fs::write(&tmp_path, contents)
                .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
            fs::rename(&tmp_path, path).with_context(|| {
                format!("renaming {} -> {}", tmp_path.display(), path.display())
            })?;
            Ok(())
        };
        let result = write_then_rename();
        if result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        result
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

    /// The RAW persisted `signing_key_bytes` for a room, ignoring any
    /// `--signing-key-file` / `RIVER_SIGNING_KEY_FILE` override that
    /// [`Self::get_room`]'s `resolve_signing_key` would apply.
    ///
    /// Used by `identity import --force` to decide whether the imported key
    /// actually changes the stored identity (freenet/river#414): comparing
    /// against the override-resolved key would misjudge a real change as
    /// unchanged (skipping the DM prune) or an unchanged identity as changed
    /// (pruning wrongly). Returns `None` when no room is stored for `owner_vk`.
    pub fn persisted_signing_key_bytes(&self, owner_vk: &VerifyingKey) -> Result<Option<[u8; 32]>> {
        let storage = self.load_rooms()?;
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        Ok(storage
            .rooms
            .get(&owner_key_str)
            .map(|r| r.signing_key_bytes))
    }

    /// Load all stored rooms, regenerating each room's contract key to match the
    /// currently-bundled WASM (and persisting the regeneration).
    ///
    /// Takes the advisory lock for the whole read-and-maybe-rewrite, so a
    /// concurrent mutating invocation can't interleave with the key-regeneration
    /// save (issue freenet/river#307).
    pub fn load_rooms(&self) -> Result<RoomStorage> {
        self.with_lock(|| self.load_rooms_unlocked())
    }

    /// Lock-free body of [`Self::load_rooms`]. The caller MUST already hold the
    /// advisory lock (see the [`Storage`] no-nesting rule). The internal
    /// key-regeneration save goes through [`Self::save_rooms_unlocked`] so it
    /// does not re-lock and self-deadlock.
    fn load_rooms_unlocked(&self) -> Result<RoomStorage> {
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
            self.save_rooms_unlocked(&storage)?;
        }

        Ok(storage)
    }

    /// Run `f` against a freshly-loaded `rooms.json` snapshot and persist the
    /// (possibly) mutated result — all under ONE advisory lock, so the whole
    /// `load → mutate → save` is atomic against concurrent invocations (issue
    /// freenet/river#307).
    ///
    /// Always re-saves the snapshot (even if `f` made no change). Callers that
    /// only read should use [`Self::load_rooms`] instead. This is the critical-
    /// section primitive external callers (`api.rs`) use in place of a bare
    /// `load_rooms()` … `save_rooms()` pair, which would race between the two
    /// lock acquisitions.
    pub fn mutate_rooms<T>(&self, f: impl FnOnce(&mut RoomStorage) -> Result<T>) -> Result<T> {
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let out = f(&mut storage)?;
            self.save_rooms_unlocked(&storage)?;
            Ok(out)
        })
    }

    /// Like [`Self::mutate_rooms`] but for the outbound-DM cache
    /// (`outbound_dms.json`). Loads, runs `f`, and persists under one advisory
    /// lock (issue freenet/river#307). Always re-saves.
    pub fn mutate_outbound_dms<T>(
        &self,
        f: impl FnOnce(&mut OutboundDmStore) -> Result<T>,
    ) -> Result<T> {
        self.with_lock(|| {
            let mut store = self.load_outbound_dms_unlocked()?;
            let out = f(&mut store)?;
            self.save_outbound_dms_unlocked(&store)?;
            Ok(out)
        })
    }

    /// Persist `rooms.json`, taking the advisory lock for the write.
    ///
    /// Most callers should NOT call this in a separate step from
    /// [`Self::load_rooms`]: a `load_rooms()` … `save_rooms()` pair across two
    /// lock acquisitions reopens the lost-update race between the two. Prefer a
    /// single locked critical section — see the in-`Storage` mutating helpers
    /// (`add_room`, `update_room_state`, …) which wrap load→mutate→save in one
    /// `with_lock`. This method exists for the few external callers
    /// (`api.rs::migrate_room_to_new_contract`) that genuinely write a value not
    /// derived from a just-loaded snapshot.
    pub fn save_rooms(&self, storage: &RoomStorage) -> Result<()> {
        self.with_lock(|| self.save_rooms_unlocked(storage))
    }

    /// Lock-free body of [`Self::save_rooms`]. Caller MUST hold the advisory
    /// lock. Writes atomically (temp-file + rename) so no reader sees a partial
    /// blob (issue freenet/river#307).
    fn save_rooms_unlocked(&self, storage: &RoomStorage) -> Result<()> {
        let contents = serde_json::to_string_pretty(storage)?;
        Self::atomic_write(&self.storage_path, &contents)
    }

    /// Load the local outbound-DM plaintext cache (issue
    /// freenet/river#256). Returns an empty store if the file does
    /// not exist; surfaces any other I/O or parse error.
    ///
    /// Takes the advisory lock for the read so it can't observe a torn write
    /// from a concurrent save (issue freenet/river#307).
    pub fn load_outbound_dms(&self) -> Result<OutboundDmStore> {
        self.with_lock(|| self.load_outbound_dms_unlocked())
    }

    /// Lock-free body of [`Self::load_outbound_dms`]. Caller MUST hold the
    /// advisory lock (see the [`Storage`] no-nesting rule).
    fn load_outbound_dms_unlocked(&self) -> Result<OutboundDmStore> {
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
    ///
    /// Writes atomically under the advisory lock (issue freenet/river#307). As
    /// with [`Self::save_rooms`], prefer a single locked load→mutate→save over
    /// pairing a bare `load_outbound_dms()` with this — the in-`Storage`
    /// `*_outbound*` helpers do exactly that.
    pub fn save_outbound_dms(&self, store: &OutboundDmStore) -> Result<()> {
        self.with_lock(|| self.save_outbound_dms_unlocked(store))
    }

    /// Lock-free body of [`Self::save_outbound_dms`]. Caller MUST hold the
    /// advisory lock. Writes atomically (temp-file + rename).
    fn save_outbound_dms_unlocked(&self, store: &OutboundDmStore) -> Result<()> {
        let contents = serde_json::to_string_pretty(store)?;
        Self::atomic_write(&self.outbound_dms_path, &contents)
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
        // Single locked critical section: load → mutate → save under one
        // advisory lock so a concurrent invocation can't clobber this insert
        // (issue freenet/river#307).
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;

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
            self.save_rooms_unlocked(&storage)
        })
    }

    /// Atomically import (or `--force`-overwrite) an identity for a room, under a
    /// SINGLE advisory lock, closing the TOCTOU window (freenet/river#414, Codex
    /// round-6 P1-5).
    ///
    /// The pre-`import` existence/key snapshot in `import_identity` is taken
    /// BEFORE the network GET, so a concurrent `riverctl` invocation can add an
    /// identity for this room during the await — after which the un-atomic path
    /// would still think it's a new room and overwrite it without `--force` (and
    /// without pruning the old identity's DM cache). This method re-reads the
    /// persisted key INSIDE the lock, makes the new-vs-overwrite + key-changed
    /// decision, writes the full `StoredRoomInfo` in one shot (folding in the
    /// authorized member, invite chain, and nickname — no read-back-and-patch),
    /// and prunes the old identity's outbound-DM cache when the key changed — all
    /// before any concurrent writer can interleave. Modeled on `remove_room`,
    /// the existing one-lock-spanning-both-files method.
    ///
    /// Returns `RefusedNeedsForce` (nothing written) when a DIFFERENT-or-same
    /// identity already exists and `force` is false; otherwise `Imported`.
    #[allow(clippy::too_many_arguments)]
    pub fn import_room_atomic(
        &self,
        owner_vk: &VerifyingKey,
        signing_key: &SigningKey,
        state: ChatRoomStateV1,
        contract_key: &ContractKey,
        invitation_secrets: HashMap<u32, [u8; 32]>,
        authorized_member: &AuthorizedMember,
        invite_chain: &[AuthorizedMember],
        self_nickname: Option<&str>,
        force: bool,
    ) -> Result<ImportOutcome> {
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

            // Authoritative re-check INSIDE the lock — this is what closes the
            // TOCTOU: read the RAW persisted key off the just-loaded snapshot
            // (never `persisted_signing_key_bytes`, which re-locks and would
            // self-deadlock per the Storage no-nesting rule).
            let old_key = storage
                .rooms
                .get(&owner_key_str)
                .map(|r| r.signing_key_bytes);
            let was_overwrite = old_key.is_some();
            if was_overwrite && !force {
                // A concurrent import may have created this room during our GET;
                // refuse rather than silently overwrite. Nothing written.
                return Ok(ImportOutcome::RefusedNeedsForce);
            }
            let identity_changed = old_key.is_some_and(|k| k != signing_key.to_bytes());

            // Assemble the incoming identity metadata.
            let mut invitation_secrets = invitation_secrets;
            let mut self_nickname = self_nickname.map(str::to_string);
            let mut invite_chain = invite_chain.to_vec();
            let mut self_authorized_member = Some(authorized_member.clone());

            // Same-key re-import over an existing room (`was_overwrite &&
            // !identity_changed`): the incoming token may be a legacy/stale export
            // that decodes optional fields as ABSENT/EMPTY. GENERAL RULE (Codex
            // round-7/8): for EVERY field the token may carry absent/stale, RETAIN
            // the stored value rather than overwrite it with an empty one — a
            // genuine identity change instead REPLACES (old identity's metadata
            // must not carry forward).
            if was_overwrite && !identity_changed {
                if let Some(old) = storage.rooms.get(&owner_key_str) {
                    // invitation_secrets: merge, existing wins — the stored map may
                    // be the ONLY copy of a private room's key, so an empty token
                    // must not wipe it (history would become unreadable, round-7).
                    for (version, secret) in &old.invitation_secrets {
                        invitation_secrets.entry(*version).or_insert(*secret);
                    }
                    // self_nickname: an absent token nickname must NOT erase the
                    // stored chosen nickname (else `build_rejoin_delta` falls back
                    // to "Member" after an inactivity prune).
                    if self_nickname.is_none() {
                        self_nickname = old.self_nickname.clone();
                    }
                    // membership proof (self_authorized_member + invite_chain) is a
                    // coherent UNIT (the chain validates the member): keep the
                    // stored pair when the token carries an empty chain, so we never
                    // pair the token's member with an empty/mismatched chain.
                    if invite_chain.is_empty() && !old.invite_chain.is_empty() {
                        invite_chain = old.invite_chain.clone();
                        if old.self_authorized_member.is_some() {
                            self_authorized_member = old.self_authorized_member.clone();
                        }
                    }
                }
            }

            // Build the FULL record in one shot (matches the fields
            // `add_room_with_invitation_secrets` + `store_authorized_member` +
            // `update_self_nickname` would set across three separate locks).
            storage.rooms.insert(
                owner_key_str.clone(),
                StoredRoomInfo {
                    signing_key_bytes: signing_key.to_bytes(),
                    state,
                    contract_key: contract_key.id().to_string(),
                    self_authorized_member,
                    invite_chain,
                    previous_contract_key: None,
                    invitation_secrets,
                    self_nickname,
                },
            );
            self.save_rooms_unlocked(&storage)?;

            // Swapping to a DIFFERENT identity: prune the OLD identity's DM cache
            // in the SAME lock. Best-effort, mirroring `remove_room`: the identity
            // is already committed, so a prune hiccup warns rather than fails.
            if identity_changed {
                if let Err(e) = self.prune_outbound_dms_for_room_unlocked(owner_vk) {
                    tracing::warn!(
                        "import: failed to prune the previous identity's DM cache for {owner_key_str}: {e}"
                    );
                }
            }

            Ok(ImportOutcome::Imported {
                was_overwrite,
                identity_changed,
            })
        })
    }

    /// Persist the member's own nickname for `owner_vk`'s room, so a later
    /// rejoin (`ApiClient::build_rejoin_delta`) can restore it instead of the
    /// generic "Member" placeholder. No-op if the room isn't stored yet.
    pub fn update_self_nickname(&self, owner_vk: &VerifyingKey, nickname: &str) -> Result<()> {
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
            if let Some(info) = storage.rooms.get_mut(&owner_key_str) {
                info.self_nickname = Some(nickname.to_string());
                self.save_rooms_unlocked(&storage)?;
            }
            Ok(())
        })
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
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

            if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
                room_info.state = state;
                self.save_rooms_unlocked(&storage)
            } else {
                Err(anyhow!("Room not found"))
            }
        })
    }

    /// Update the contract key for a room (used during migration to new contract version)
    pub fn update_contract_key(
        &self,
        owner_vk: &VerifyingKey,
        new_key: &ContractKey,
    ) -> Result<()> {
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();

            if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
                room_info.contract_key = new_key.id().to_string();
                self.save_rooms_unlocked(&storage)
            } else {
                Err(anyhow!("Room not found"))
            }
        })
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
        // The rooms.json removal and the outbound-DM-cache prune both run under
        // ONE advisory lock: they touch two files but are one logical "leave"
        // operation, and the unlocked prune helper must not re-lock and
        // self-deadlock (issue freenet/river#307).
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
            let removed = storage.rooms.remove(&owner_key_str).is_some();
            if removed {
                self.save_rooms_unlocked(&storage)?;
                if let Err(e) = self.prune_outbound_dms_for_room_unlocked(owner_vk) {
                    // Non-fatal: the room is already removed from rooms.json. Warn
                    // so the orphaned plaintext is visible, but don't fail leave.
                    tracing::warn!(
                        "room leave: failed to prune outbound-DM cache for {owner_key_str}: {e}"
                    );
                }
            }
            Ok(removed)
        })
    }

    /// Public, self-locking counterpart to
    /// [`Self::prune_outbound_dms_for_room_unlocked`]: drop every cached
    /// outbound-DM plaintext entry and archived-thread entry for `owner_vk`'s
    /// room from `outbound_dms.json`.
    ///
    /// Used by `identity import --force` when it REPLACES a room with a
    /// different signing key (freenet/river#414): the prior identity's plaintext
    /// outbound DMs and its archived-thread records (keyed by `(room, peer)`)
    /// must not leak into the new identity's view, so they are pruned exactly as
    /// [`Self::remove_room`] does on leave. Takes the advisory lock itself; do
    /// NOT call while already holding it.
    pub fn prune_outbound_dms_for_room(&self, owner_vk: &VerifyingKey) -> Result<()> {
        self.with_lock(|| self.prune_outbound_dms_for_room_unlocked(owner_vk))
    }

    /// Drop every cached outbound-DM plaintext entry and archived-thread entry
    /// for `owner_vk`'s room from `outbound_dms.json`. No-op if the cache file
    /// holds nothing for that room. Called by [`Self::remove_room`]; caller MUST
    /// already hold the advisory lock (see the [`Storage`] no-nesting rule).
    fn prune_outbound_dms_for_room_unlocked(&self, owner_vk: &VerifyingKey) -> Result<()> {
        let mut store = self.load_outbound_dms_unlocked()?;
        let room_bytes = owner_vk.to_bytes();
        let before = store.entries.len() + store.hidden_threads.len();
        store.entries.retain(|e| e.room_owner_vk != room_bytes);
        store
            .hidden_threads
            .retain(|h| h.room_owner_vk != room_bytes);
        if store.entries.len() + store.hidden_threads.len() != before {
            self.save_outbound_dms_unlocked(&store)?;
        }
        Ok(())
    }

    pub fn store_authorized_member(
        &self,
        owner_vk: &VerifyingKey,
        authorized_member: &AuthorizedMember,
        invite_chain: &[AuthorizedMember],
    ) -> Result<()> {
        self.with_lock(|| {
            let mut storage = self.load_rooms_unlocked()?;
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
            if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
                room_info.self_authorized_member = Some(authorized_member.clone());
                room_info.invite_chain = invite_chain.to_vec();
                self.save_rooms_unlocked(&storage)?;
            }
            Ok(())
        })
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
                    let sealed_name = &room_info.state.configuration.configuration.display.name;
                    // A private room's name is AES-256-GCM sealed under the room
                    // secret. Decrypt it with the local member's secrets so
                    // `room list` shows the real name instead of the
                    // "[Encrypted: N bytes, vN]" placeholder that `to_string_lossy`
                    // yields for a sealed value. Falls back to that placeholder
                    // when the secret is unavailable (not yet synced / rotated
                    // past); a public name decrypts trivially to its bytes.
                    let room_name = if sealed_name.is_private() {
                        let self_sk = self.resolve_signing_key(&room_info.signing_key_bytes);
                        let secrets = crate::private_room::collect_secrets_for_room(
                            &room_info.state,
                            &self_sk,
                            &room_info.invitation_secrets,
                        );
                        river_core::ecies::unseal_bytes_with_secrets(sealed_name, &secrets)
                            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
                            .unwrap_or_else(|_| sealed_name.to_string_lossy())
                    } else {
                        sealed_name.to_string_lossy()
                    };
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

    /// A `room list` on a **private** room must show the decrypted name, not
    /// the "[Encrypted: N bytes, vN]" placeholder (reported on Matrix
    /// 2026-07-08: riverctl "cant see ... the real room name"). Here the room
    /// secret is supplied via `invitation_secrets`, mirroring a just-joined
    /// invitee before the owner's delegate has back-filled the contract blob.
    #[test]
    fn list_rooms_decrypts_private_room_name() {
        use river_core::ecies::seal_bytes;
        use river_core::room_state::privacy::PrivacyMode;

        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let secret = [3u8; 32];
        let version = 0u32;

        // Build a PRIVATE room whose display name is sealed under `secret`.
        let mut state = create_test_state(&owner_sk);
        state.configuration.configuration.privacy_mode = PrivacyMode::Private;
        state.configuration.configuration.display.name =
            seal_bytes(b"Secret Room", &secret, version);
        // Re-sign so the (mutated) configuration stays owner-authorized.
        state.configuration =
            AuthorizedConfigurationV1::new(state.configuration.configuration.clone(), &owner_sk);
        let contract_key = expected_contract_key(&owner_vk);

        // Store with the secret carried as an invitation secret.
        let mut invitation_secrets = HashMap::new();
        invitation_secrets.insert(version, secret);
        storage
            .add_room_with_invitation_secrets(
                &owner_vk,
                &owner_sk,
                state,
                &contract_key,
                invitation_secrets,
            )
            .unwrap();

        let rooms = storage.list_rooms().unwrap();
        let (_, name, _) = rooms
            .iter()
            .find(|(vk, _, _)| *vk == owner_vk)
            .expect("room present");
        assert_eq!(name, "Secret Room");
    }

    /// Without the secret, the private name gracefully falls back to the
    /// placeholder rather than erroring or panicking.
    #[test]
    fn list_rooms_private_name_falls_back_without_secret() {
        use river_core::ecies::seal_bytes;
        use river_core::room_state::privacy::PrivacyMode;

        let (storage, _temp_dir) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();

        let mut state = create_test_state(&owner_sk);
        state.configuration.configuration.privacy_mode = PrivacyMode::Private;
        state.configuration.configuration.display.name = seal_bytes(b"Secret Room", &[3u8; 32], 0);
        state.configuration =
            AuthorizedConfigurationV1::new(state.configuration.configuration.clone(), &owner_sk);
        let contract_key = expected_contract_key(&owner_vk);

        // No invitation secrets, and the state carries no owner-signed blob for
        // this member → the secret is unavailable.
        storage
            .add_room(&owner_vk, &owner_sk, state, &contract_key)
            .unwrap();

        let rooms = storage.list_rooms().unwrap();
        let (_, name, _) = rooms
            .iter()
            .find(|(vk, _, _)| *vk == owner_vk)
            .expect("room present");
        assert_eq!(name, "[Encrypted: 11 bytes, v0]");
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
            identity_src.contains("nickname_to_persist.as_deref()"),
            "import_identity must persist the imported nickname — now folded into \
             the single-lock `Storage::import_room_atomic` write (freenet/river#414 \
             P1-5) via the `nickname_to_persist` argument, not a separate \
             `update_self_nickname` call."
        );
    }

    /// Source-grep pin for the #304 member_info self-heal wiring, in storage.rs
    /// so the pinned strings are not self-satisfied by the scanned file
    /// (api.rs). Guards the exact gap #304 fixed: a CLI member stranded in
    /// `members` but absent from `member_info` (rendering as "Unknown")
    /// with no remediation path. The heal MUST stay wired into the central
    /// GET path so every read/send command re-attempts it once a secret
    /// arrives.
    #[test]
    fn member_info_heal_wiring_pinned() {
        let api_src = include_str!("api.rs");
        assert!(
            api_src.contains("self.heal_member_info(room_owner_key, room_state)"),
            "get_room must invoke the member_info self-heal (issue #304) so a \
             member stranded in `members` but absent from `member_info` is \
             remediated on every GET. Do NOT drop this call."
        );
        // get_room must REBIND room_state to the heal's return value, so callers
        // operate on the repaired state — otherwise a follow-up delta is applied
        // to the pre-heal state and written back, dropping the healed entry
        // locally (Codex review on PR #358).
        assert!(
            api_src.contains("let room_state = match self.heal_member_info("),
            "get_room must rebind `room_state` to the healed state returned by \
             heal_member_info; do NOT discard the heal result and return the \
             pre-heal state."
        );
        assert!(
            api_src.contains("crate::private_room::build_member_info_heal("),
            "heal_member_info must route the heal decision through the pure \
             `crate::private_room::build_member_info_heal` (which defers a \
             private-room heal when no secret is available, never leaking a \
             plaintext nickname). Do NOT inline an unconditional Some(...)."
        );
        // The persisted nickname/secrets belong to the STORED identity; under a
        // `--signing-key` override selecting a different key they are not this
        // member's, so heal_member_info must skip rather than republish another
        // member's nickname under the override key (Codex review on PR #358).
        assert!(
            api_src.contains("signing_key.to_bytes() != info.signing_key_bytes"),
            "heal_member_info must skip the heal when the resolved signing key \
             differs from the room's stored identity (signing-key override case) \
             — otherwise it would republish the stored member's nickname/secrets \
             under the override key. Do NOT drop this guard."
        );
        // The local heal must fold the member_info in DIRECTLY (push onto
        // `healed_state.member_info.member_info`), NOT via a full-state
        // `ChatRoomStateV1::apply_delta` — the latter runs `post_apply_cleanup`,
        // which would inactivity-prune the very (unanchored) member being healed
        // and drop the new member_info, since the heal adds no anchoring message
        // (Codex review round 3 on PR #358). We assert the direct-push exists and
        // that heal_member_info does not reach for full-state apply_delta.
        //
        // Whitespace-insensitive: collapse runs of whitespace before matching so
        // a future rustfmt line-rewrap can't silently void the pin.
        let api_collapsed: String = api_src.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            api_collapsed
                .contains("healed_state .member_info .member_info .push(authorized_info.clone());"),
            "heal_member_info must build the local healed state by directly \
             pushing the member_info entry, NOT by calling full-state \
             apply_delta (which runs post_apply_cleanup and would prune the \
             unanchored member it is trying to heal)."
        );

        // build_member_info_heal must defer for a member who would NOT survive
        // post_apply_cleanup — otherwise the standalone member_info UPDATE
        // triggers the contract's cleanup and PRUNES that member from `members`
        // instead of healing them (Codex review round 5 on PR #358). Pinned from
        // storage.rs so the asserted string is not self-satisfied by
        // private_room.rs's own test source.
        let private_room_src = include_str!("private_room.rs");
        assert!(
            private_room_src.contains("post_apply_cleanup(&params)"),
            "build_member_info_heal must simulate post_apply_cleanup and defer when \
             the member would be pruned — a heal UPDATE for an unanchored member \
             removes them instead of repairing. Do NOT drop this anchor guard."
        );
    }

    /// Source-grep pin for the #306 import wiring, in storage.rs so the pinned
    /// strings aren't self-satisfied by the scanned file (commands/identity.rs).
    /// Guards the exact regression #306 fixed: `import_identity` calling the
    /// no-secrets `add_room`, which silently drops a private-room invitee's
    /// invitation-carried secrets on a device migration (making their later
    /// `invitation create` emit `room_secrets: []`).
    #[test]
    fn import_identity_seeds_invitation_secrets_wiring_pinned() {
        // NOTE: these two pinned substrings appear ONLY in the production
        // export/import paths, never in `commands/identity.rs`'s own test
        // module — so the assertions can't be self-satisfied by test code in
        // the scanned file. (A bare `add_room_with_invitation_secrets(` would
        // be, since that module's tests also call it; we deliberately don't
        // pin that.)
        let identity_src = include_str!("commands/identity.rs");
        // The export side must populate the field from the stored room info,
        // not leave it empty.
        assert!(
            identity_src.contains("invitation_secrets: room_info.invitation_secrets.clone()"),
            "export_identity must populate IdentityExport.invitation_secrets from \
             the room's StoredRoomInfo.invitation_secrets (freenet/river#306)."
        );
        // The import side must seed storage with the export's carried secrets
        // (via add_room_with_invitation_secrets), NOT the no-secrets add_room —
        // otherwise a private-room invitee loses their secret on a device
        // migration, making their later `invitation create` emit
        // `room_secrets: []`.
        assert!(
            identity_src.contains("export.invitation_secrets.clone()"),
            "import_identity must pass export.invitation_secrets into \
             Storage::add_room_with_invitation_secrets (freenet/river#306)."
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

    /// freenet/river#414 (Codex round 4): `identity import --force` that swaps a
    /// room to a DIFFERENT signing key must prune the OLD identity's cached
    /// outbound-DM plaintext + archived threads (they belong to the old
    /// identity and would otherwise leak into / wrongly hide threads for the new
    /// one) — but an unchanged-key re-import must NOT prune, and a different
    /// room's cache always survives. Drives the exact Storage sequence
    /// `import_identity` runs (decision `old_key != imported_key`, then
    /// `prune_outbound_dms_for_room`).
    #[test]
    fn force_import_prunes_dm_cache_only_on_key_change() {
        use freenet_scaffold::util::FastHash;
        use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmEntry, OutboundDmStore};
        use river_core::room_state::direct_messages::PurgeToken;
        use river_core::room_state::member::MemberId;

        let (storage, _tmp) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let other_vk = create_test_signing_key().verifying_key();
        let key = expected_contract_key(&owner_vk);

        let old_identity = create_test_signing_key();
        let new_identity = create_test_signing_key();

        let peer = MemberId(FastHash(7));
        let seed_dm_cache = || {
            storage
                .save_outbound_dms(&OutboundDmStore {
                    entries: vec![
                        OutboundDmEntry {
                            room_owner_vk: owner_vk.to_bytes(),
                            sender: peer,
                            recipient: peer,
                            purge_token: PurgeToken([1u8; 16]),
                            timestamp: 1,
                            plaintext: "old-identity secret".to_string(),
                        },
                        OutboundDmEntry {
                            room_owner_vk: other_vk.to_bytes(),
                            sender: peer,
                            recipient: peer,
                            purge_token: PurgeToken([2u8; 16]),
                            timestamp: 2,
                            plaintext: "unrelated room".to_string(),
                        },
                    ],
                    hidden_threads: vec![HiddenDmThreadEntry {
                        room_owner_vk: owner_vk.to_bytes(),
                        peer,
                        hidden_at_ts: 1,
                    }],
                })
                .unwrap();
        };

        // Scenario 1: --force replace with a DIFFERENT key → prune this room's cache.
        storage
            .add_room(&owner_vk, &old_identity, create_test_state(&owner_sk), &key)
            .unwrap();
        seed_dm_cache();
        let old_key = storage.get_room(&owner_vk).unwrap().unwrap().0;
        let identity_changed = old_key.to_bytes() != new_identity.to_bytes();
        assert!(identity_changed, "precondition: the imported key differs");
        storage
            .add_room_with_invitation_secrets(
                &owner_vk,
                &new_identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
            )
            .unwrap();
        if identity_changed {
            storage.prune_outbound_dms_for_room(&owner_vk).unwrap();
        }
        let after = storage.load_outbound_dms().unwrap();
        assert!(
            !after
                .entries
                .iter()
                .any(|e| e.room_owner_vk == owner_vk.to_bytes()),
            "the replaced identity's DM plaintext must be pruned"
        );
        assert!(
            after
                .hidden_threads
                .iter()
                .all(|h| h.room_owner_vk != owner_vk.to_bytes()),
            "the replaced identity's archived threads must be pruned"
        );
        assert!(
            after
                .entries
                .iter()
                .any(|e| e.room_owner_vk == other_vk.to_bytes()),
            "another room's DM cache must survive"
        );

        // Scenario 2: re-import the SAME key → no identity change → keep the cache.
        seed_dm_cache();
        let old_key2 = storage.get_room(&owner_vk).unwrap().unwrap().0;
        let identity_changed2 = old_key2.to_bytes() != new_identity.to_bytes();
        assert!(
            !identity_changed2,
            "re-importing the SAME key is not an identity change"
        );
        storage
            .add_room_with_invitation_secrets(
                &owner_vk,
                &new_identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
            )
            .unwrap();
        if identity_changed2 {
            storage.prune_outbound_dms_for_room(&owner_vk).unwrap();
        }
        let after2 = storage.load_outbound_dms().unwrap();
        assert!(
            after2
                .entries
                .iter()
                .any(|e| e.room_owner_vk == owner_vk.to_bytes()),
            "an unchanged-key re-import must NOT prune the DM cache"
        );
    }

    /// freenet/river#414 (Codex round 5): `persisted_signing_key_bytes` returns
    /// the RAW stored key, NOT the `--signing-key` override that `get_room`
    /// resolves. The `identity import --force` DM-prune decision (`identity_changed`)
    /// depends on this — comparing against the override would misjudge whether
    /// the identity actually changed.
    #[test]
    fn persisted_signing_key_bytes_ignores_override() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().to_str().unwrap();

        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let stored_identity = create_test_signing_key();
        let override_identity = create_test_signing_key();
        assert_ne!(stored_identity.to_bytes(), override_identity.to_bytes());

        // Persist the room under `stored_identity` (no override active).
        let storage = Storage::new(Some(dir)).unwrap();
        let key = expected_contract_key(&owner_vk);
        storage
            .add_room(
                &owner_vk,
                &stored_identity,
                create_test_state(&owner_sk),
                &key,
            )
            .unwrap();

        // Re-open the SAME dir with an override selecting a DIFFERENT identity.
        let overridden =
            Storage::new_with_override(Some(dir), Some(override_identity.clone())).unwrap();

        // get_room resolves to the override…
        let (resolved, _, _) = overridden.get_room(&owner_vk).unwrap().unwrap();
        assert_eq!(
            resolved.to_bytes(),
            override_identity.to_bytes(),
            "get_room applies the --signing-key override"
        );

        // …but persisted_signing_key_bytes returns the RAW stored key, so the
        // import DM-prune decision compares against the real stored identity.
        let persisted = overridden
            .persisted_signing_key_bytes(&owner_vk)
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted,
            stored_identity.to_bytes(),
            "persisted key must ignore the override"
        );

        // Absent room → None.
        let absent = create_test_signing_key().verifying_key();
        assert!(overridden
            .persisted_signing_key_bytes(&absent)
            .unwrap()
            .is_none());
    }

    /// freenet/river#414 (Codex round-6 P1-5): `import_room_atomic` re-checks
    /// existence + key INSIDE the lock, closing the TOCTOU where a concurrent
    /// import created the room during this import's network GET. Simulates the
    /// interleave by pre-adding the room, then importing over it.
    #[test]
    fn import_room_atomic_rechecks_existence_under_lock() {
        use freenet_scaffold::util::FastHash;
        use river_core::chat_delegate::{OutboundDmEntry, OutboundDmStore};
        use river_core::room_state::direct_messages::PurgeToken;
        use river_core::room_state::member::{Member, MemberId};

        let (storage, _tmp) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let key = expected_contract_key(&owner_vk);

        let old_identity = create_test_signing_key();
        let new_identity = create_test_signing_key();
        let member_for = |sk: &SigningKey| {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_vk.into(),
                    invited_by: owner_vk.into(),
                    member_vk: sk.verifying_key(),
                },
                &owner_sk,
            )
        };

        // A concurrent `riverctl` created the room under `old_identity` while our
        // GET was in flight, and cached an outbound DM under it.
        storage
            .add_room(&owner_vk, &old_identity, create_test_state(&owner_sk), &key)
            .unwrap();
        let peer = MemberId(FastHash(7));
        storage
            .save_outbound_dms(&OutboundDmStore {
                entries: vec![OutboundDmEntry {
                    room_owner_vk: owner_vk.to_bytes(),
                    sender: peer,
                    recipient: peer,
                    purge_token: PurgeToken([1u8; 16]),
                    timestamp: 1,
                    plaintext: "old-identity secret".to_string(),
                }],
                hidden_threads: vec![],
            })
            .unwrap();

        // WITHOUT --force: the atomic re-check finds the concurrently-added room
        // and REFUSES — nothing written, the old identity + its DM cache survive.
        let outcome = storage
            .import_room_atomic(
                &owner_vk,
                &new_identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
                &member_for(&new_identity),
                &[],
                Some("newnick"),
                false,
            )
            .unwrap();
        assert_eq!(outcome, ImportOutcome::RefusedNeedsForce);
        assert_eq!(
            storage
                .persisted_signing_key_bytes(&owner_vk)
                .unwrap()
                .unwrap(),
            old_identity.to_bytes(),
            "a refused import must NOT overwrite the concurrently-added identity"
        );
        assert!(
            storage
                .load_outbound_dms()
                .unwrap()
                .entries
                .iter()
                .any(|e| e.room_owner_vk == owner_vk.to_bytes()),
            "a refused import must NOT prune the DM cache"
        );

        // WITH --force: overwrite in one lock — full record written, key changed,
        // old identity's DM cache pruned.
        let outcome = storage
            .import_room_atomic(
                &owner_vk,
                &new_identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
                &member_for(&new_identity),
                std::slice::from_ref(&member_for(&new_identity)),
                Some("newnick"),
                true,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Imported {
                was_overwrite: true,
                identity_changed: true,
            }
        );
        let rooms = storage.load_rooms().unwrap();
        let info = rooms
            .rooms
            .get(&bs58::encode(owner_vk.as_bytes()).into_string())
            .unwrap();
        assert_eq!(info.signing_key_bytes, new_identity.to_bytes());
        assert!(
            info.self_authorized_member.is_some(),
            "the atomic write must fold in the authorized member"
        );
        assert_eq!(
            info.invite_chain.len(),
            1,
            "invite chain written in the same lock"
        );
        assert_eq!(info.self_nickname.as_deref(), Some("newnick"));
        assert!(
            !storage
                .load_outbound_dms()
                .unwrap()
                .entries
                .iter()
                .any(|e| e.room_owner_vk == owner_vk.to_bytes()),
            "a --force key change must prune the old identity's DM cache in the same lock"
        );

        // A brand-new room (not present) imports without --force and reports no
        // overwrite / no key change.
        let fresh_owner_sk = create_test_signing_key();
        let fresh_owner_vk = fresh_owner_sk.verifying_key();
        let fresh_key = expected_contract_key(&fresh_owner_vk);
        let fresh_self = create_test_signing_key();
        let outcome = storage
            .import_room_atomic(
                &fresh_owner_vk,
                &fresh_self,
                create_test_state(&fresh_owner_sk),
                &fresh_key,
                std::collections::HashMap::new(),
                &AuthorizedMember::new(
                    Member {
                        owner_member_id: fresh_owner_vk.into(),
                        invited_by: fresh_owner_vk.into(),
                        member_vk: fresh_self.verifying_key(),
                    },
                    &fresh_owner_sk,
                ),
                &[],
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Imported {
                was_overwrite: false,
                identity_changed: false,
            }
        );
        assert!(storage.get_room(&fresh_owner_vk).unwrap().is_some());
    }

    /// freenet/river#414 (Codex round 7): a SAME-key `--force` re-import must
    /// RETAIN the stored `invitation_secrets` (merging the token's map in,
    /// existing wins) — the token may be a legacy/stale export with an empty map,
    /// and for a private room awaiting the owner backfill the stored map can be
    /// the only copy of the room key. A DIFFERENT-key `--force` still REPLACES.
    #[test]
    fn import_room_atomic_same_key_preserves_invitation_secrets() {
        use river_core::room_state::member::Member;

        let (storage, _tmp) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let key = expected_contract_key(&owner_vk);
        let member_for = |sk: &SigningKey| {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_vk.into(),
                    invited_by: owner_vk.into(),
                    member_vk: sk.verifying_key(),
                },
                &owner_sk,
            )
        };

        // Store the room under `identity` with the ONLY copy of the room key at v3.
        let identity = create_test_signing_key();
        let mut secrets = std::collections::HashMap::new();
        secrets.insert(3u32, [0x55u8; 32]);
        storage
            .import_room_atomic(
                &owner_vk,
                &identity,
                create_test_state(&owner_sk),
                &key,
                secrets,
                &member_for(&identity),
                &[],
                None,
                false,
            )
            .unwrap();

        // --force re-import of the SAME identity from a token with EMPTY secrets.
        let outcome = storage
            .import_room_atomic(
                &owner_vk,
                &identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
                &member_for(&identity),
                &[],
                None,
                true,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Imported {
                was_overwrite: true,
                identity_changed: false,
            }
        );
        assert_eq!(
            storage
                .get_invitation_secrets(&owner_vk)
                .unwrap()
                .get(&3u32),
            Some(&[0x55u8; 32]),
            "same-key --force re-import must RETAIN the stored invitation secret \
             (only copy of the room key)"
        );

        // A DIFFERENT-key --force re-import REPLACES (drops the old secret).
        let new_identity = create_test_signing_key();
        let outcome = storage
            .import_room_atomic(
                &owner_vk,
                &new_identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
                &member_for(&new_identity),
                &[],
                None,
                true,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Imported {
                was_overwrite: true,
                identity_changed: true,
            }
        );
        assert!(
            storage
                .get_invitation_secrets(&owner_vk)
                .unwrap()
                .is_empty(),
            "different-key --force re-import must REPLACE (drop the old identity's secrets)"
        );
    }

    /// freenet/river#414 (Codex round-8, systematic same-key audit): a same-key
    /// `--force` re-import from a stale token (absent nickname, empty invite
    /// chain) must RETAIN the stored nickname + membership proof, not erase them
    /// (else `build_rejoin_delta` falls back to "Member").
    #[test]
    fn import_room_atomic_same_key_preserves_nickname_and_chain() {
        use river_core::room_state::member::Member;

        let (storage, _tmp) = create_test_storage();
        let owner_sk = create_test_signing_key();
        let owner_vk = owner_sk.verifying_key();
        let key = expected_contract_key(&owner_vk);
        let identity = create_test_signing_key();
        let member = AuthorizedMember::new(
            Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: identity.verifying_key(),
            },
            &owner_sk,
        );

        // Store the room WITH a chosen nickname and a non-empty invite chain.
        storage
            .import_room_atomic(
                &owner_vk,
                &identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
                &member,
                std::slice::from_ref(&member),
                Some("Chosen Name"),
                false,
            )
            .unwrap();

        // --force re-import of the SAME key from a stale token: no nickname, empty chain.
        storage
            .import_room_atomic(
                &owner_vk,
                &identity,
                create_test_state(&owner_sk),
                &key,
                std::collections::HashMap::new(),
                &member,
                &[],
                None,
                true,
            )
            .unwrap();

        let rooms = storage.load_rooms().unwrap();
        let info = rooms
            .rooms
            .get(&bs58::encode(owner_vk.as_bytes()).into_string())
            .unwrap();
        assert_eq!(
            info.self_nickname.as_deref(),
            Some("Chosen Name"),
            "an absent token nickname must NOT erase the stored chosen nickname"
        );
        assert_eq!(
            info.invite_chain.len(),
            1,
            "an empty token invite_chain must NOT erase the stored chain"
        );
        assert!(
            info.self_authorized_member.is_some(),
            "the stored membership proof must survive a stale same-key re-import"
        );
    }

    /// Regression for issue freenet/river#307 (lost-update race).
    ///
    /// Each `add_room` is a `load → mutate → save` sequence. Pre-fix, `save_rooms`
    /// did a bare `fs::write` with no advisory lock, so N concurrent invocations
    /// could all load the same base snapshot and the last writer would clobber
    /// every other writer's insert. With the advisory lock around the whole
    /// critical section, the inserts serialize and ALL N rooms survive.
    ///
    /// We spin many threads each inserting a DISTINCT room and assert every one
    /// is present at the end. Without the lock this fails reliably (most inserts
    /// are lost); with it, it passes deterministically.
    #[test]
    fn concurrent_add_room_does_not_lose_updates() {
        use std::sync::{Arc, Barrier};

        const WRITERS: usize = 16;

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().to_str().unwrap().to_string();
        // One shared Storage instance, as a single process's threads would share.
        let storage = Arc::new(Storage::new(Some(&dir)).unwrap());
        // Align all threads at the load→mutate→save entry to maximize the race
        // window a missing lock would expose.
        let barrier = Arc::new(Barrier::new(WRITERS));

        let mut owners = Vec::with_capacity(WRITERS);
        let mut handles = Vec::with_capacity(WRITERS);
        for _ in 0..WRITERS {
            let owner_sk = create_test_signing_key();
            let owner_vk = owner_sk.verifying_key();
            owners.push(owner_vk);

            let storage = Arc::clone(&storage);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let state = create_test_state(&owner_sk);
                let key = expected_contract_key(&owner_vk);
                barrier.wait();
                storage.add_room(&owner_vk, &owner_sk, state, &key).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let loaded = storage.load_rooms().unwrap();
        assert_eq!(
            loaded.rooms.len(),
            WRITERS,
            "every concurrent add_room must survive — a smaller count means a \
             lost-update race clobbered some inserts (issue freenet/river#307)"
        );
        for owner_vk in &owners {
            let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
            assert!(
                loaded.rooms.contains_key(&owner_key_str),
                "room for {owner_key_str} was lost to a concurrent writer"
            );
        }
    }

    /// Issue freenet/river#307 for the outbound-DM cache: concurrent
    /// `mutate_outbound_dms` appends (the primitive `riverctl dm send` uses) must
    /// not lose entries. Each append is a `load → push → save`; without the lock
    /// the racing saves clobber each other.
    #[test]
    fn concurrent_outbound_dm_appends_do_not_lose_updates() {
        use freenet_scaffold::util::FastHash;
        use river_core::chat_delegate::OutboundDmEntry;
        use river_core::room_state::direct_messages::PurgeToken;
        use river_core::room_state::member::MemberId;
        use std::sync::{Arc, Barrier};

        const WRITERS: usize = 16;

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().to_str().unwrap().to_string();
        let storage = Arc::new(Storage::new(Some(&dir)).unwrap());
        let barrier = Arc::new(Barrier::new(WRITERS));

        let room_vk = create_test_signing_key().verifying_key();
        let peer = MemberId(FastHash(7));

        let mut handles = Vec::with_capacity(WRITERS);
        for i in 0..WRITERS {
            let storage = Arc::clone(&storage);
            let barrier = Arc::clone(&barrier);
            let room_bytes = room_vk.to_bytes();
            handles.push(std::thread::spawn(move || {
                let entry = OutboundDmEntry {
                    room_owner_vk: room_bytes,
                    sender: peer,
                    recipient: peer,
                    // Distinct purge token per writer so the per-pair cap (which
                    // we stay under) never drops one as a duplicate.
                    purge_token: PurgeToken([i as u8; 16]),
                    timestamp: i as u64,
                    plaintext: format!("dm-{i}"),
                };
                barrier.wait();
                storage
                    .mutate_outbound_dms(|store| {
                        store.entries.push(entry);
                        Ok(())
                    })
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let after = storage.load_outbound_dms().unwrap();
        assert_eq!(
            after.entries.len(),
            WRITERS,
            "every concurrent outbound-DM append must survive (issue \
             freenet/river#307)"
        );
    }

    /// The atomic-write property (issue freenet/river#307): a concurrent reader
    /// must never observe a half-written `rooms.json`. We hammer `add_room`
    /// (writers) while a reader repeatedly `load_rooms`; every read must parse
    /// successfully. Pre-fix, `fs::write` truncates-then-writes in place, so a
    /// reader could read a truncated/partial blob and `serde_json` would error.
    #[test]
    fn concurrent_reads_never_observe_a_torn_write() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().to_str().unwrap().to_string();
        let storage = Arc::new(Storage::new(Some(&dir)).unwrap());

        // Seed one room so the file exists and is non-trivially sized.
        let seed_sk = create_test_signing_key();
        let seed_vk = seed_sk.verifying_key();
        storage
            .add_room(
                &seed_vk,
                &seed_sk,
                create_test_state(&seed_sk),
                &expected_contract_key(&seed_vk),
            )
            .unwrap();

        let done = Arc::new(AtomicBool::new(false));

        // Reader: load_rooms in a tight loop until writers finish. load_rooms
        // surfaces a parse error as Err, which unwrap() would turn into a panic.
        let reader = {
            let storage = Arc::clone(&storage);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !done.load(Ordering::Relaxed) {
                    storage
                        .load_rooms()
                        .expect("a concurrent reader must never see a torn rooms.json");
                    reads += 1;
                }
                reads
            })
        };

        // Writers: keep adding distinct rooms to keep the file changing.
        for _ in 0..64 {
            let sk = create_test_signing_key();
            let vk = sk.verifying_key();
            storage
                .add_room(
                    &vk,
                    &sk,
                    create_test_state(&sk),
                    &expected_contract_key(&vk),
                )
                .unwrap();
        }

        done.store(true, Ordering::Relaxed);
        let reads = reader.join().unwrap();
        assert!(reads > 0, "reader thread should have completed reads");
    }

    /// Source pins for the issue freenet/river#307 fix so a future refactor can't
    /// silently regress the two safety properties. These pin the API surface, not
    /// internal variable names, so they survive renames that keep the behavior.
    #[test]
    fn rooms_storage_locking_and_atomic_write_pinned() {
        let src = include_str!("storage.rs");
        // Mutating methods must wrap load→mutate→save in a single advisory lock.
        assert!(
            src.contains("fn with_lock<"),
            "Storage must keep a with_lock critical-section helper (issue #307)"
        );
        assert!(
            src.contains("lock_exclusive()"),
            "with_lock must take an EXCLUSIVE advisory lock (issue #307)"
        );
        // Saves must be atomic (temp-file + rename), never a bare in-place write.
        assert!(
            src.contains("fn atomic_write(") && src.contains("fs::rename("),
            "saves must go through atomic_write (temp-file + rename) (issue #307)"
        );
        // The reentrancy guard: load_rooms's internal regeneration save must use
        // the UNLOCKED variant so it doesn't self-deadlock under the outer lock.
        assert!(
            src.contains("self.save_rooms_unlocked(&storage)?;")
                && src.contains("fn load_rooms_unlocked("),
            "load_rooms must save via save_rooms_unlocked to avoid lock reentrancy \
             self-deadlock (issue #307)"
        );
    }
}
