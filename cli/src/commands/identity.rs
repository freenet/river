use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::identity::IdentityExport;
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::ChatRoomParametersV1;

#[derive(Subcommand)]
pub enum IdentityCommands {
    /// Export your identity for a room as a portable token
    Export {
        /// Room owner's verifying key (base58)
        room: String,
    },
    /// Import an identity from a portable token
    Import {
        /// The armored identity token (reads from stdin if not provided)
        #[arg(long)]
        token: Option<String>,
        /// Path to a file containing the armored identity token
        #[arg(long, conflicts_with = "token")]
        file: Option<String>,
        /// Replace an existing identity for the room instead of refusing.
        ///
        /// Overwriting loses the current identity's signing key locally unless
        /// it was exported first (`riverctl identity export`). The room's
        /// messages/members live on the network and re-sync (freenet/river#414).
        #[arg(long, visible_alias = "overwrite")]
        force: bool,
    },
}

pub async fn execute(
    command: IdentityCommands,
    api_client: ApiClient,
    format: OutputFormat,
) -> Result<()> {
    match command {
        IdentityCommands::Export { room } => export_identity(&api_client, &room, format).await,
        IdentityCommands::Import { token, file, force } => {
            import_identity(&api_client, token, file, force, format).await
        }
    }
}

async fn export_identity(
    api_client: &ApiClient,
    room_key_str: &str,
    format: OutputFormat,
) -> Result<()> {
    let room_owner_key = parse_room_key(room_key_str)?;

    // Get signing key and stored data from local storage
    let room_data = api_client
        .storage()
        .get_room(&room_owner_key)?
        .ok_or_else(|| {
            anyhow!("Room not found in local storage. You must be a member of this room.")
        })?;
    let (signing_key, _, _contract_key_str) = room_data;

    // Get self_authorized_member and invite_chain from storage
    let storage = api_client.storage().load_rooms()?;
    let key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
    let room_info = storage
        .rooms
        .get(&key_str)
        .ok_or_else(|| anyhow!("Room data not found in storage"))?;

    let is_owner = signing_key.verifying_key() == room_owner_key;

    // Resolve AuthorizedMember and invite chain:
    // 1. Use cached self_authorized_member if available
    // 2. For owners: create a self-signed AuthorizedMember
    // 3. For non-owners: look up from network state
    let (authorized_member, invite_chain) =
        if let Some(am) = room_info.self_authorized_member.clone() {
            (am, room_info.invite_chain.clone())
        } else if is_owner {
            let owner_id = MemberId::from(&room_owner_key);
            let member = Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: room_owner_key,
            };
            (AuthorizedMember::new(member, &signing_key), vec![])
        } else {
            // Try fetching from network state
            let state = api_client
                .get_room(&room_owner_key, false)
                .await
                .map_err(|_| {
                    anyhow!(
                        "No authorized member data cached and could not fetch from network. \
                     Try sending a message first."
                    )
                })?;
            let vk = signing_key.verifying_key();
            let params = ChatRoomParametersV1 {
                owner: room_owner_key,
            };
            let m = state
                .members
                .members
                .iter()
                .find(|m| m.member.member_vk == vk)
                .ok_or_else(|| {
                    anyhow!(
                        "You are not in this room's member list. \
                         Try sending a message first to populate membership data."
                    )
                })?;
            let chain = state
                .members
                .get_invite_chain(m, &params)
                .map_err(|e| anyhow!("Could not resolve invite chain: {}", e))?;
            (m.clone(), chain)
        };

    // Fetch fresh state from network to get current member_info (nickname) and room name
    let (member_info, room_name) = match api_client.get_room(&room_owner_key, false).await {
        Ok(room_state) => {
            let self_id = MemberId::from(&signing_key.verifying_key());
            // `canonical`, not a bare `.find()` (#411 round 8 item A): a state
            // can hold more than one member_info record for self, and a bare
            // first-match could export a losing (revoked) duplicate.
            let info = room_state.member_info.canonical(self_id).cloned();
            let name = match &room_state.configuration.configuration.display.name {
                river_core::room_state::privacy::SealedBytes::Public { value } => {
                    Some(String::from_utf8_lossy(value).to_string())
                }
                _ => None, // Private rooms: can't decrypt without secrets
            };
            (info, name)
        }
        Err(_) => (None, None), // Network unavailable; export without extras
    };

    // Wire-format safety check. The export's signing_key MUST match the
    // authorized_member.member.member_vk; otherwise importing the token
    // produces an identity whose secret key signs nothing the room
    // contract accepts.
    check_export_coherence(
        &signing_key,
        &authorized_member,
        api_client.storage().has_signing_key_override(),
    )?;

    let export = IdentityExport {
        room_owner: room_owner_key,
        signing_key,
        authorized_member,
        invite_chain,
        member_info,
        room_name,
        // Carry the chosen nickname in plaintext so an export taken before
        // the private-room join-heal sealed `member_info` doesn't lose it on
        // re-import (freenet/river#298).
        self_nickname: room_info.self_nickname.clone(),
        // Carry the invitation-carried room secrets so a non-owner of a
        // private room can still forward useful `room_secrets` via
        // `invitation create` after re-importing on another device
        // (freenet/river#306). Empty for public rooms and for owners.
        invitation_secrets: room_info.invitation_secrets.clone(),
    };

    let armored = export.to_armored_string();

    match format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "room": key_str,
                "token": armored,
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        OutputFormat::Human => {
            eprintln!("WARNING: This token contains your private key. Treat it like a password.");
            eprintln!();
            println!("{}", armored);
        }
    }

    Ok(())
}

async fn import_identity(
    api_client: &ApiClient,
    token: Option<String>,
    file: Option<String>,
    force: bool,
    format: OutputFormat,
) -> Result<()> {
    // Read token from argument, file, or stdin
    let armored = if let Some(t) = token {
        t
    } else if let Some(path) = file {
        std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("Failed to read file '{}': {}", path, e))?
    } else {
        // Read from stdin
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow!("Failed to read from stdin: {}", e))?;
        buf
    };

    let export = IdentityExport::from_armored_string(&armored)
        .map_err(|e| anyhow!("Invalid identity token: {}", e))?;

    let room_key_str = bs58::encode(export.room_owner.as_bytes()).into_string();

    // Fast-path pre-check (freenet/river#414): compare against the RAW persisted
    // key (NOT `get_room`'s `--signing-key-file` override-resolved key) so we can
    // skip a pointless network GET when we'd only refuse, and emit the replace
    // warning. This is ADVISORY — the authoritative new-vs-overwrite + key-changed
    // decision is re-made atomically INSIDE `import_room_atomic`'s lock AFTER the
    // GET, so a concurrent `riverctl` import during the await cannot make us
    // overwrite (or skip the DM prune) on a stale view (Codex round-6 P1-5).
    let persisted_old_key = api_client
        .storage()
        .persisted_signing_key_bytes(&export.room_owner)?;
    if let Some(refusal) =
        import_overwrite_refusal(persisted_old_key.is_some(), force, &room_key_str)
    {
        return Err(anyhow!(refusal));
    }
    if persisted_old_key.is_some() {
        // force && exists: proceeding to replace — warn about the lost key.
        eprintln!(
            "WARNING: replacing the existing identity for room {}. The previous \
             signing key is lost unless you exported it first.",
            room_key_str
        );
    }

    // Fetch room state from network to populate local storage
    let room_state = api_client
        .get_room(&export.room_owner, false)
        .await
        .map_err(|e| {
            anyhow!(
                "Failed to fetch room state from network: {}. Is your Freenet node running?",
                e
            )
        })?;

    // Compute the nickname to persist BEFORE the atomic write so it goes into the
    // same `StoredRoomInfo` under one lock (a later rejoin restores it instead of
    // "Member"). Prefer the public-plaintext `member_info` nickname; else the
    // plaintext `self_nickname` the export carries. A private room's sealed
    // `member_info` nickname renders as an "[Encrypted: …]" placeholder via
    // `to_string_lossy` (worse than the fallback), so it is excluded here.
    let public_member_info_nickname = export.member_info.as_ref().and_then(|info| {
        info.member_info
            .preferred_nickname
            .is_public()
            .then(|| info.member_info.preferred_nickname.to_string_lossy())
    });
    let nickname_to_persist = public_member_info_nickname
        .clone()
        .or_else(|| export.self_nickname.clone());

    // Atomic import (freenet/river#414 P1-5): re-check existence + key and write
    // the full record (room + authorized member + invite chain + nickname) under
    // ONE lock, pruning the old identity's DM cache on a key change — all inside
    // the lock, so a concurrent import can't interleave. Seeds `invitation_secrets`
    // from the export so a non-owner of a private room keeps the secret across a
    // device migration (freenet/river#306).
    let contract_key = api_client.owner_vk_to_contract_key(&export.room_owner);
    let outcome = api_client.storage().import_room_atomic(
        &export.room_owner,
        &export.signing_key,
        room_state,
        &contract_key,
        export.invitation_secrets.clone(),
        &export.authorized_member,
        &export.invite_chain,
        nickname_to_persist.as_deref(),
        force,
    )?;

    if outcome == crate::storage::ImportOutcome::RefusedNeedsForce {
        // A concurrent import created this room during our network GET — refuse
        // rather than silently overwrite it (this is the TOCTOU we now catch).
        return Err(anyhow!(import_overwrite_refusal(
            true,
            false,
            &room_key_str
        )
        .expect(
            "refusal message is Some for an existing room without --force"
        )));
    }

    // For the human/JSON summary, show the real nickname: prefer the
    // public-plaintext `member_info` nickname, then the carried plaintext
    // `self_nickname`, then "unknown". (A sealed private `member_info`
    // nickname renders as an "[Encrypted: …]" placeholder via
    // `to_string_lossy`, so the `self_nickname` fallback is more useful.)
    let nickname = public_member_info_nickname
        .or_else(|| export.self_nickname.clone())
        .or_else(|| {
            export
                .member_info
                .as_ref()
                .map(|i| i.member_info.preferred_nickname.to_string_lossy())
        })
        .unwrap_or_else(|| "unknown".to_string());

    match format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "status": "imported",
                "room": room_key_str,
                "nickname": nickname,
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        OutputFormat::Human => {
            println!("Identity imported successfully for room {}", room_key_str);
            println!("Nickname: {}", nickname);
        }
    }

    Ok(())
}

/// Decide whether an identity import must be refused because the room already
/// has a stored identity and the user did not opt into replacing it.
///
/// Returns the refusal message when the import should be blocked (an identity
/// exists and `force` is false), or `None` when the import may proceed —
/// either the room is new, or `--force`/`--overwrite` authorizes the replace
/// (freenet/river#414). Pure, so the guard is unit-testable without a node.
fn import_overwrite_refusal(room_exists: bool, force: bool, room_key_str: &str) -> Option<String> {
    if room_exists && !force {
        Some(format!(
            "You already have an identity for room {room}. \
             Re-run with `--force` (alias `--overwrite`) to replace it, or \
             remove it first with `riverctl room leave {room}`. Replacing loses \
             your current signing key for this room unless you exported it \
             first with `riverctl identity export {room}`.",
            room = room_key_str
        ))
    } else {
        None
    }
}

fn parse_room_key(s: &str) -> Result<VerifyingKey> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| anyhow!("Invalid base58 room key: {}", e))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("Room key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow!("Invalid verifying key: {}", e))
}

/// Wire-format coherence guard for identity export.
///
/// The export's `signing_key` MUST match `authorized_member.member.member_vk`;
/// otherwise importing the token produces an identity whose secret key signs
/// nothing the room contract accepts. The guard itself is unconditional — only
/// the diagnostic hint adapts to context.
///
/// The hint branches on whether a signing-key override is active:
/// - **Override set**: the usual cause is `--signing-key-file` /
///   `RIVER_SIGNING_KEY_FILE` pointing at one identity while `rooms.json` still
///   holds another identity's cached `AuthorizedMember`. Tell the user to drop
///   or re-point the override.
/// - **No override**: the override hint would mislead — the check can also fire
///   when `rooms.json` is internally inconsistent (e.g. the chat-delegate sync
///   wrote `signing_key_bytes` for one identity but `self_authorized_member`
///   for another, the bug-class that motivated this guard). Point the user at
///   the corruption instead of an override they never set.
fn check_export_coherence(
    signing_key: &SigningKey,
    authorized_member: &AuthorizedMember,
    has_signing_key_override: bool,
) -> Result<()> {
    if signing_key.verifying_key() == authorized_member.member.member_vk {
        return Ok(());
    }

    let hint = if has_signing_key_override {
        "This usually happens when `--signing-key-file` / `RIVER_SIGNING_KEY_FILE` \
         overrides the signing identity but `rooms.json` still holds another \
         identity's cached membership state. Re-run without the override (or with \
         the override pointing at THIS identity) to produce a coherent token."
    } else {
        "`rooms.json` appears corrupted: its cached AuthorizedMember.member_vk does \
         not match the room's stored signing key. Try re-accepting the invitation, \
         or import a fresh identity token for this room."
    };

    Err(anyhow!(
        "Refusing to export an identity with mismatched signing key and \
         authorized member. The signing key's verifying key does not match the \
         cached AuthorizedMember.member_vk for this room. {hint}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::compute_contract_key;
    use crate::storage::Storage;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::member::{Member, MemberId};
    use river_core::room_state::ChatRoomStateV1;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn signing_key_from_seed(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// Minimal valid room state owned by `owner_sk` (matches the helper in
    /// `storage.rs` tests).
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

    /// End-to-end round-trip for freenet/river#306: a non-owner export that
    /// carries `invitation_secrets` must, after armor → decode → the
    /// storage-seeding step `import_identity` performs, leave the secrets
    /// retrievable via `get_invitation_secrets`. This is the exact wiring the
    /// issue asks for (export populates the field, import seeds storage via
    /// `add_room_with_invitation_secrets`), exercised without a live node by
    /// driving the same `Storage` call the import path uses.
    #[test]
    fn invitation_secrets_survive_export_armor_import_persist() {
        let owner_sk = signing_key_from_seed(7);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);

        // A non-owner member invited by the owner.
        let member_sk = signing_key_from_seed(8);
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        };
        let authorized_member = AuthorizedMember::new(member, &owner_sk);

        let mut invitation_secrets: HashMap<u32, [u8; 32]> = HashMap::new();
        invitation_secrets.insert(0, [0xABu8; 32]);
        invitation_secrets.insert(2, [0xCDu8; 32]);

        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: member_sk.clone(),
            authorized_member,
            invite_chain: vec![],
            member_info: None,
            room_name: None,
            self_nickname: None,
            invitation_secrets: invitation_secrets.clone(),
        };

        // Export → armor → wipe → decode, exactly as a device migration would.
        let armored = export.to_armored_string();
        let decoded = IdentityExport::from_armored_string(&armored).unwrap();

        // Import-persist step: a fresh storage (the "new device") seeds the
        // room from the decoded export, mirroring `import_identity`.
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        let contract_key = compute_contract_key(&decoded.room_owner);
        storage
            .add_room_with_invitation_secrets(
                &decoded.room_owner,
                &decoded.signing_key,
                create_test_state(&owner_sk),
                &contract_key,
                decoded.invitation_secrets.clone(),
            )
            .unwrap();

        // The persisted secrets must match the originals byte-for-byte. Before
        // the #306 fix the export dropped the field and this returned empty.
        let retrieved = storage.get_invitation_secrets(&owner_vk).unwrap();
        assert_eq!(
            retrieved, invitation_secrets,
            "invitation_secrets must survive export → import → persist"
        );
    }

    /// A public-room (or pre-#306) export carries no secrets; the import path
    /// must persist an empty map, not panic or invent entries.
    #[test]
    fn empty_invitation_secrets_round_trip_to_empty() {
        let owner_sk = signing_key_from_seed(9);
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: owner_vk,
        };
        let authorized_member = AuthorizedMember::new(member, &owner_sk);

        let export = IdentityExport {
            room_owner: owner_vk,
            signing_key: owner_sk.clone(),
            authorized_member,
            invite_chain: vec![],
            member_info: None,
            room_name: None,
            self_nickname: None,
            invitation_secrets: HashMap::new(),
        };

        let armored = export.to_armored_string();
        let decoded = IdentityExport::from_armored_string(&armored).unwrap();
        assert!(decoded.invitation_secrets.is_empty());

        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        let contract_key = compute_contract_key(&decoded.room_owner);
        storage
            .add_room_with_invitation_secrets(
                &decoded.room_owner,
                &decoded.signing_key,
                create_test_state(&owner_sk),
                &contract_key,
                decoded.invitation_secrets.clone(),
            )
            .unwrap();

        assert!(storage
            .get_invitation_secrets(&owner_vk)
            .unwrap()
            .is_empty());
    }

    /// Build an `AuthorizedMember` signed by `key` (the member is `key`'s own
    /// identity, mirroring an owner/self-signed membership).
    fn authorized_member_for(key: &SigningKey) -> AuthorizedMember {
        let vk = key.verifying_key();
        let id = MemberId::from(&vk);
        let member = Member {
            owner_member_id: id,
            invited_by: id,
            member_vk: vk,
        };
        AuthorizedMember::new(member, key)
    }

    #[test]
    fn coherence_passes_when_signing_key_matches_member() {
        let key = signing_key_from_seed(1);
        let am = authorized_member_for(&key);
        // Both override and no-override must accept a coherent pair.
        assert!(check_export_coherence(&key, &am, true).is_ok());
        assert!(check_export_coherence(&key, &am, false).is_ok());
    }

    #[test]
    fn coherence_error_mentions_override_when_override_set() {
        // AuthorizedMember signed by key A; export attempted with key B as the
        // active override → mismatch, override-set wording.
        let key_a = signing_key_from_seed(1);
        let key_b = signing_key_from_seed(2);
        assert_ne!(key_a.to_bytes(), key_b.to_bytes());
        let am = authorized_member_for(&key_a);

        let err =
            check_export_coherence(&key_b, &am, true).expect_err("mismatched keys must error");
        let msg = err.to_string();
        assert!(
            msg.contains("--signing-key-file") || msg.contains("RIVER_SIGNING_KEY_FILE"),
            "override-set hint should reference the override mechanism, got: {msg}"
        );
        assert!(
            !msg.contains("corrupted"),
            "override-set hint should not blame corruption, got: {msg}"
        );
    }

    #[test]
    fn coherence_error_mentions_corruption_when_no_override() {
        // Same mismatch, but no override active → corruption wording, and it
        // must NOT tell the user to drop an override they never set.
        let key_a = signing_key_from_seed(1);
        let key_b = signing_key_from_seed(2);
        let am = authorized_member_for(&key_a);

        let err =
            check_export_coherence(&key_b, &am, false).expect_err("mismatched keys must error");
        let msg = err.to_string();
        assert!(
            msg.contains("corrupted"),
            "no-override hint should point at corruption, got: {msg}"
        );
        assert!(
            !msg.contains("--signing-key-file") && !msg.contains("RIVER_SIGNING_KEY_FILE"),
            "no-override hint must not reference an override the user never set, got: {msg}"
        );
    }

    /// freenet/river#414: without `--force`, importing over an existing room
    /// refuses — and the refusal must name `--force`, its `--overwrite` alias,
    /// and the room. With `--force`, or for a brand-new room, it proceeds.
    #[test]
    fn import_overwrite_refusal_matrix() {
        let room = "ExampleRoomKey58";

        // Brand-new room: never refuse, force or not.
        assert!(import_overwrite_refusal(false, false, room).is_none());
        assert!(import_overwrite_refusal(false, true, room).is_none());

        // Existing room, no force: refuse with the improved message.
        let msg = import_overwrite_refusal(true, false, room).expect("must refuse without --force");
        assert!(
            msg.contains("--force"),
            "refusal must point at --force, got: {msg}"
        );
        assert!(
            msg.contains("--overwrite"),
            "refusal must mention the --overwrite alias, got: {msg}"
        );
        assert!(
            msg.contains(room),
            "refusal should name the room, got: {msg}"
        );

        // Existing room, force: proceed (no refusal).
        assert!(
            import_overwrite_refusal(true, true, room).is_none(),
            "--force must authorize replacing an existing identity"
        );
    }

    /// freenet/river#414: a `--force` import runs the same storage sequence the
    /// import path uses, but with a new signing key — it must REPLACE the
    /// stored identity in place (not error, not duplicate). Drives the `Storage`
    /// calls directly (the async `import_identity` needs a live node).
    #[test]
    fn force_import_overwrites_stored_identity() {
        let owner_sk = signing_key_from_seed(21);
        let owner_vk = owner_sk.verifying_key();

        let old_sk = signing_key_from_seed(22);
        let new_sk = signing_key_from_seed(23);
        assert_ne!(old_sk.to_bytes(), new_sk.to_bytes());

        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::new(Some(temp_dir.path().to_str().unwrap())).unwrap();
        let contract_key = compute_contract_key(&owner_vk);

        // First import establishes the OLD identity.
        storage
            .add_room(
                &owner_vk,
                &old_sk,
                create_test_state(&owner_sk),
                &contract_key,
            )
            .unwrap();
        let (stored, _, _) = storage.get_room(&owner_vk).unwrap().unwrap();
        assert_eq!(
            stored.to_bytes(),
            old_sk.to_bytes(),
            "precondition: OLD identity is stored"
        );

        // The `--force` path re-runs the import's storage sequence with the NEW
        // key: add_room_with_invitation_secrets (overwrites) + store_authorized_member.
        storage
            .add_room_with_invitation_secrets(
                &owner_vk,
                &new_sk,
                create_test_state(&owner_sk),
                &contract_key,
                HashMap::new(),
            )
            .unwrap();
        storage
            .store_authorized_member(&owner_vk, &authorized_member_for(&new_sk), &[])
            .unwrap();

        let (stored, _, _) = storage.get_room(&owner_vk).unwrap().unwrap();
        assert_eq!(
            stored.to_bytes(),
            new_sk.to_bytes(),
            "force import must overwrite the stored signing key with the imported one"
        );
        assert_eq!(
            storage.load_rooms().unwrap().rooms.len(),
            1,
            "overwrite replaces in place — no duplicate room entry"
        );
    }
}
