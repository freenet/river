use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use colored::Colorize;

#[derive(Subcommand)]
pub enum MemberCommands {
    /// List members of a room
    List {
        /// Room ID (owner key in base58)
        room_id: String,
    },
    /// Set your nickname in a room
    SetNickname {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Your new nickname
        nickname: String,
    },
    /// Ban a member from a room
    Ban {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID to ban (8-character short ID from member list)
        member_id: String,
    },
    /// Deputize a member so they can help moderate (ban) within your invite subtree
    Deputize {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID to deputize (8-character short ID from member list)
        member_id: String,
    },
    /// Revoke a member's deputy authority (their prior bans stop enforcing)
    RevokeDeputy {
        /// Room ID (owner key in base58)
        room_id: String,
        /// Member ID whose deputy authority to revoke (8-character short ID)
        member_id: String,
    },
}

pub async fn execute(command: MemberCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        MemberCommands::List { room_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Listing members of room: {}", room_id);
            }

            // Parse the room owner key
            let owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
            if owner_key_bytes.len() != 32 {
                return Err(anyhow!("Invalid room ID: expected 32 bytes"));
            }
            let mut key_array = [0u8; 32];
            key_array.copy_from_slice(&owner_key_bytes);
            let owner_vk = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;

            // Get the room state
            let mut room_state = api.get_room(&owner_vk, false).await?;

            // For a private room, collect the local member's secrets so member
            // nicknames (which are AES-256-GCM sealed) decrypt instead of
            // rendering as "[Encrypted: N bytes, vN]". Empty / no-op for a
            // public room or a room not in local storage.
            let secrets = api.room_display_secrets(&owner_vk, &mut room_state);

            // Collect member info
            let members: Vec<_> = room_state
                .member_info
                .member_info
                .iter()
                .map(|info| {
                    let nickname = crate::api::unseal_nickname_display(
                        &info.member_info.preferred_nickname,
                        &secrets,
                    );
                    let member_id = info.member_info.member_id.to_string();
                    (member_id, nickname)
                })
                .collect();

            match format {
                OutputFormat::Human => {
                    if members.is_empty() {
                        println!("No members found in room.");
                    } else {
                        println!("\n{} member(s) found:\n", members.len());
                        for (member_id, nickname) in members {
                            println!("  {} ({})", nickname.green(), &member_id[..8]);
                        }
                        println!();
                    }
                }
                OutputFormat::Json => {
                    let json_members: Vec<_> = members
                        .into_iter()
                        .map(|(member_id, nickname)| {
                            serde_json::json!({
                                "member_id": member_id,
                                "nickname": nickname,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json_members)?);
                }
            }
            Ok(())
        }
        MemberCommands::SetNickname { room_id, nickname } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Setting nickname to '{}' in room: {}", nickname, room_id);
            }

            // Parse the room owner key
            let owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
            if owner_key_bytes.len() != 32 {
                return Err(anyhow!("Invalid room ID: expected 32 bytes"));
            }
            let mut key_array = [0u8; 32];
            key_array.copy_from_slice(&owner_key_bytes);
            let owner_vk = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;

            match api.set_nickname(&owner_vk, nickname.clone()).await {
                Ok(()) => match format {
                    OutputFormat::Human => {
                        println!("{}", "Nickname updated successfully!".green());
                    }
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "success": true,
                                "nickname": nickname,
                            })
                        );
                    }
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::Ban { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Banning member '{}' from room: {}", member_id, room_id);
            }

            // Parse the room owner key
            let owner_key_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
            if owner_key_bytes.len() != 32 {
                return Err(anyhow!("Invalid room ID: expected 32 bytes"));
            }
            let mut key_array = [0u8; 32];
            key_array.copy_from_slice(&owner_key_bytes);
            let owner_vk = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
                .map_err(|e| anyhow!("Invalid room ID: {}", e))?;

            match api.ban_member(&owner_vk, &member_id).await {
                Ok(()) => match format {
                    OutputFormat::Human => {
                        println!(
                            "{}",
                            format!("Member '{}' has been banned.", member_id).green()
                        );
                    }
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "success": true,
                                "banned_member_id": member_id,
                            })
                        );
                    }
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::Deputize { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Deputizing member '{}' in room: {}", member_id, room_id);
            }
            let owner_vk = parse_room_id(&room_id)?;
            match api.deputize(&owner_vk, &member_id).await {
                Ok(()) => match format {
                    OutputFormat::Human => println!(
                        "{}",
                        format!(
                            "Member '{}' can now help moderate people you invited.",
                            member_id
                        )
                        .green()
                    ),
                    OutputFormat::Json => println!(
                        "{}",
                        serde_json::json!({ "success": true, "deputized_member_id": member_id })
                    ),
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
        MemberCommands::RevokeDeputy { room_id, member_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Revoking deputy '{}' in room: {}", member_id, room_id);
            }
            let owner_vk = parse_room_id(&room_id)?;
            match api.revoke_deputy(&owner_vk, &member_id).await {
                Ok(()) => match format {
                    OutputFormat::Human => println!(
                        "{}",
                        format!("Member '{}' is no longer your deputy.", member_id).green()
                    ),
                    OutputFormat::Json => println!(
                        "{}",
                        serde_json::json!({ "success": true, "revoked_member_id": member_id })
                    ),
                },
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    return Err(e);
                }
            }
            Ok(())
        }
    }
}

/// Decode a base58 room id (owner verifying key) into a `VerifyingKey`.
fn parse_room_id(room_id: &str) -> Result<ed25519_dalek::VerifyingKey> {
    let owner_key_bytes = bs58::decode(room_id)
        .into_vec()
        .map_err(|e| anyhow!("Invalid room ID: {}", e))?;
    if owner_key_bytes.len() != 32 {
        return Err(anyhow!("Invalid room ID: expected 32 bytes"));
    }
    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&owner_key_bytes);
    ed25519_dalek::VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow!("Invalid room ID: {}", e))
}
