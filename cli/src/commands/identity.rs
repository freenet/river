use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::identity::IdentityExport;
use river_core::room_state::member::MemberId;

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

    let authorized_member = room_info.self_authorized_member.clone().ok_or_else(|| {
        anyhow!(
            "No authorized member data found. This can happen if you created this room \
             before the membership tracking feature was added. Try sending a message first \
             to populate the membership data."
        )
    })?;

    // Fetch fresh state from network to get current member_info (nickname)
    let member_info = match api_client.get_room(&room_owner_key, false).await {
        Ok(room_state) => {
            let self_id = MemberId::from(&signing_key.verifying_key());
            room_state
                .member_info
                .member_info
                .into_iter()
                .find(|i| i.member_info.member_id == self_id)
        }
        Err(_) => None, // Network unavailable; export without member_info
    };

    let export = IdentityExport {
        room_owner: room_owner_key,
        signing_key,
        authorized_member,
        invite_chain: room_info.invite_chain.clone(),
        member_info,
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

    let nickname = export
        .member_info
        .as_ref()
        .map(|i| i.member_info.preferred_nickname.to_string_lossy())
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
