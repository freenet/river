use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
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
    },
}

pub async fn execute(
    command: IdentityCommands,
    api_client: ApiClient,
    format: OutputFormat,
) -> Result<()> {
    match command {
        IdentityCommands::Export { room } => export_identity(&api_client, &room, format).await,
        IdentityCommands::Import { token, file } => {
            import_identity(&api_client, token, file, format).await
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
            let info = room_state
                .member_info
                .member_info
                .iter()
                .find(|i| i.member_info.member_id == self_id)
                .cloned();
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
    //
    // This catches the case where `--signing-key-file` overrides the
    // signing identity but `self_authorized_member` is read from
    // `rooms.json` (which still has the previous identity's
    // `AuthorizedMember`). The two pieces would otherwise be packaged
    // together with mismatched verifying keys, silently breaking the
    // imported identity on first use.
    if signing_key.verifying_key() != authorized_member.member.member_vk {
        return Err(anyhow!(
            "Refusing to export an identity with mismatched signing key and \
             authorized member. The signing key's verifying key does not match \
             the cached AuthorizedMember.member_vk for this room. This usually \
             happens when `--signing-key-file` / `RIVER_SIGNING_KEY_FILE` overrides \
             the signing identity but `rooms.json` still holds another identity's \
             cached membership state. Re-run without the override (or with the \
             override pointing at THIS identity) to produce a coherent token."
        ));
    }

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

    // Check if we already have this room
    if api_client.storage().get_room(&export.room_owner)?.is_some() {
        return Err(anyhow!(
            "You already have an identity for room {}. \
             Remove it first with `riverctl room leave {}` if you want to replace it.",
            room_key_str,
            room_key_str
        ));
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

    // Store the room with the imported identity
    let contract_key = api_client.owner_vk_to_contract_key(&export.room_owner);
    api_client.storage().add_room(
        &export.room_owner,
        &export.signing_key,
        room_state,
        &contract_key,
    )?;

    // Store the authorized member and invite chain for rejoin support
    api_client.storage().store_authorized_member(
        &export.room_owner,
        &export.authorized_member,
        &export.invite_chain,
    )?;

    // Persist the imported nickname so a later rejoin (after an inactivity
    // prune) restores it instead of "Member".
    //
    // Prefer the public-plaintext nickname carried in `member_info`. A
    // private room's exported `member_info` nickname is sealed, so
    // `to_string_lossy` yields an "[Encrypted: …]" placeholder, not the real
    // name; persisting that would be worse than the generic fallback.
    //
    // When `member_info` carries no usable public nickname (it is absent —
    // e.g. an export taken before the private-room join-heal sealed it,
    // freenet/river#298 — or sealed), fall back to the plaintext
    // `self_nickname` the export now carries. This is the chosen nickname,
    // not an "[Encrypted: …]" placeholder, so it is safe to persist.
    let public_member_info_nickname = export.member_info.as_ref().and_then(|info| {
        info.member_info
            .preferred_nickname
            .is_public()
            .then(|| info.member_info.preferred_nickname.to_string_lossy())
    });
    let nickname_to_persist = public_member_info_nickname
        .clone()
        .or_else(|| export.self_nickname.clone());
    if let Some(nick) = nickname_to_persist {
        api_client
            .storage()
            .update_self_nickname(&export.room_owner, &nick)?;
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

fn parse_room_key(s: &str) -> Result<VerifyingKey> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| anyhow!("Invalid base58 room key: {}", e))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("Room key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow!("Invalid verifying key: {}", e))
}
