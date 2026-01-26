use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;
use serde::Serialize;

#[derive(Subcommand)]
pub enum DebugCommands {
    /// Perform a raw contract GET operation
    ContractGet {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Test WebSocket connection
    Websocket,
    /// Show contract key for a room
    ContractKey {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Show room state summary including bans, members, and configuration
    RoomState {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Show current ban list for a room
    Bans {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
    /// Show room configuration
    Config {
        /// Room owner key (base58 encoded)
        room_owner_key: String,
    },
}

#[derive(Serialize)]
struct BanInfo {
    banned_user_id: String,
    banned_by_id: String,
    banned_at_secs: u64,
}

#[derive(Serialize)]
struct RoomStateSummary {
    room_name: String,
    member_count: usize,
    ban_count: usize,
    message_count: usize,
    max_user_bans: usize,
    max_members: usize,
    privacy_mode: String,
    configuration_version: u32,
}

#[derive(Serialize)]
struct RoomConfig {
    room_name: String,
    privacy_mode: String,
    configuration_version: u32,
    max_recent_messages: usize,
    max_user_bans: usize,
    max_message_size: usize,
    max_nickname_size: usize,
    max_members: usize,
    max_room_name: usize,
    max_room_description: usize,
}

pub async fn execute(command: DebugCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        DebugCommands::ContractGet { room_owner_key } => {
            // Decode the room owner key from base58
            let decoded = bs58::decode(&room_owner_key)
                .into_vec()
                .map_err(|e| anyhow!("Failed to decode room owner key: {}", e))?;

            if decoded.len() != 32 {
                return Err(anyhow!(
                    "Invalid room owner key length: expected 32 bytes, got {}",
                    decoded.len()
                ));
            }

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&decoded);
            let owner_vk = VerifyingKey::from_bytes(&key_bytes)
                .map_err(|e| anyhow!("Invalid verifying key: {}", e))?;

            let contract_key = api.owner_vk_to_contract_key(&owner_vk);

            if !matches!(format, OutputFormat::Json) {
                eprintln!(
                    "DEBUG: Performing contract GET for room owned by: {}",
                    room_owner_key
                );
                eprintln!("Contract key: {}", contract_key.id());
            }

            match api.get_room(&owner_vk, false).await {
                Ok(room_state) => {
                    match format {
                        OutputFormat::Human => {
                            println!("✓ Successfully retrieved room state");
                            println!(
                                "Configuration version: {}",
                                room_state.configuration.configuration.configuration_version
                            );
                            println!(
                                "Room name: {}",
                                room_state
                                    .configuration
                                    .configuration
                                    .display
                                    .name
                                    .to_string_lossy()
                            );
                            println!("Members: {}", room_state.members.members.len());
                            println!("Messages: {}", room_state.recent_messages.messages.len());
                        }
                        OutputFormat::Json => {
                            // TODO: Implement proper JSON serialization of room state
                            println!(
                                r#"{{"status": "success", "contract_key": "{}"}}"#,
                                contract_key.id()
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    match format {
                        OutputFormat::Human => eprintln!("✗ Contract GET failed: {}", e),
                        OutputFormat::Json => {
                            println!(r#"{{"status": "error", "message": "{}"}}"#, e);
                        }
                    }
                    Err(e)
                }
            }
        }
        DebugCommands::Websocket => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("DEBUG: Testing WebSocket connection...");
            }

            match api.test_connection().await {
                Ok(()) => {
                    match format {
                        OutputFormat::Human => println!("✓ WebSocket connection successful"),
                        OutputFormat::Json => {
                            println!(
                                r#"{{"status": "success", "message": "WebSocket connection successful"}}"#
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    match format {
                        OutputFormat::Human => eprintln!("✗ WebSocket connection failed: {}", e),
                        OutputFormat::Json => {
                            println!(r#"{{"status": "error", "message": "{}"}}"#, e);
                        }
                    }
                    Err(e)
                }
            }
        }
        DebugCommands::ContractKey { room_owner_key } => {
            // Decode the room owner key from base58
            let decoded = bs58::decode(&room_owner_key)
                .into_vec()
                .map_err(|e| anyhow!("Failed to decode room owner key: {}", e))?;

            if decoded.len() != 32 {
                return Err(anyhow!(
                    "Invalid room owner key length: expected 32 bytes, got {}",
                    decoded.len()
                ));
            }

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&decoded);
            let owner_vk = VerifyingKey::from_bytes(&key_bytes)
                .map_err(|e| anyhow!("Invalid verifying key: {}", e))?;

            let contract_key = api.owner_vk_to_contract_key(&owner_vk);

            match format {
                OutputFormat::Human => {
                    println!("Room owner key: {}", room_owner_key);
                    println!("Contract key: {}", contract_key.id());
                }
                OutputFormat::Json => {
                    println!(
                        r#"{{"room_owner_key": "{}", "contract_key": "{}"}}"#,
                        room_owner_key,
                        contract_key.id()
                    );
                }
            }
            Ok(())
        }
        DebugCommands::RoomState { room_owner_key } => {
            let owner_vk = parse_owner_key(&room_owner_key)?;
            let room_state = api.get_room(&owner_vk, false).await?;

            let config = &room_state.configuration.configuration;
            let summary = RoomStateSummary {
                room_name: config.display.name.to_string_lossy(),
                member_count: room_state.members.members.len(),
                ban_count: room_state.bans.0.len(),
                message_count: room_state.recent_messages.messages.len(),
                max_user_bans: config.max_user_bans,
                max_members: config.max_members,
                privacy_mode: format!("{:?}", config.privacy_mode),
                configuration_version: config.configuration_version,
            };

            match format {
                OutputFormat::Human => {
                    println!("Room State Summary");
                    println!("==================");
                    println!("Room name: {}", summary.room_name);
                    println!("Privacy mode: {}", summary.privacy_mode);
                    println!("Config version: {}", summary.configuration_version);
                    println!();
                    println!(
                        "Members: {} / {}",
                        summary.member_count, summary.max_members
                    );
                    println!("Bans: {} / {}", summary.ban_count, summary.max_user_bans);
                    println!("Messages: {}", summary.message_count);
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                }
            }
            Ok(())
        }
        DebugCommands::Bans { room_owner_key } => {
            let owner_vk = parse_owner_key(&room_owner_key)?;
            let room_state = api.get_room(&owner_vk, false).await?;

            let bans: Vec<BanInfo> = room_state
                .bans
                .0
                .iter()
                .map(|ban| {
                    let banned_at_secs = ban
                        .ban
                        .banned_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    BanInfo {
                        banned_user_id: ban.ban.banned_user.to_string(),
                        banned_by_id: ban.banned_by.to_string(),
                        banned_at_secs,
                    }
                })
                .collect();

            match format {
                OutputFormat::Human => {
                    println!("Ban List ({} bans)", bans.len());
                    println!("=========");
                    if bans.is_empty() {
                        println!("No bans.");
                    } else {
                        for ban in &bans {
                            println!(
                                "  {} banned by {} at {}",
                                ban.banned_user_id, ban.banned_by_id, ban.banned_at_secs
                            );
                        }
                    }
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&bans)?);
                }
            }
            Ok(())
        }
        DebugCommands::Config { room_owner_key } => {
            let owner_vk = parse_owner_key(&room_owner_key)?;
            let room_state = api.get_room(&owner_vk, false).await?;

            let config = &room_state.configuration.configuration;
            let room_config = RoomConfig {
                room_name: config.display.name.to_string_lossy(),
                privacy_mode: format!("{:?}", config.privacy_mode),
                configuration_version: config.configuration_version,
                max_recent_messages: config.max_recent_messages,
                max_user_bans: config.max_user_bans,
                max_message_size: config.max_message_size,
                max_nickname_size: config.max_nickname_size,
                max_members: config.max_members,
                max_room_name: config.max_room_name,
                max_room_description: config.max_room_description,
            };

            match format {
                OutputFormat::Human => {
                    println!("Room Configuration");
                    println!("==================");
                    println!("Room name: {}", room_config.room_name);
                    println!("Privacy mode: {}", room_config.privacy_mode);
                    println!("Config version: {}", room_config.configuration_version);
                    println!();
                    println!("Limits:");
                    println!("  max_members: {}", room_config.max_members);
                    println!("  max_user_bans: {}", room_config.max_user_bans);
                    println!("  max_recent_messages: {}", room_config.max_recent_messages);
                    println!("  max_message_size: {}", room_config.max_message_size);
                    println!("  max_nickname_size: {}", room_config.max_nickname_size);
                    println!("  max_room_name: {}", room_config.max_room_name);
                    println!(
                        "  max_room_description: {}",
                        room_config.max_room_description
                    );
                }
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&room_config)?);
                }
            }
            Ok(())
        }
    }
}

/// Helper to parse a base58-encoded room owner key
fn parse_owner_key(room_owner_key: &str) -> Result<VerifyingKey> {
    let decoded = bs58::decode(room_owner_key)
        .into_vec()
        .map_err(|e| anyhow!("Failed to decode room owner key: {}", e))?;

    if decoded.len() != 32 {
        return Err(anyhow!(
            "Invalid room owner key length: expected 32 bytes, got {}",
            decoded.len()
        ));
    }

    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&decoded);
    VerifyingKey::from_bytes(&key_bytes).map_err(|e| anyhow!("Invalid verifying key: {}", e))
}
