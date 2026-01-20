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
            let room_state = api.get_room(&owner_vk, false).await?;

            // Collect member info
            let members: Vec<_> = room_state
                .member_info
                .member_info
                .iter()
                .map(|info| {
                    let nickname = info.member_info.preferred_nickname.to_string_lossy();
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
    }
}
