use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use ed25519_dalek::VerifyingKey;

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

            println!(
                "DEBUG: Performing contract GET for room owned by: {}",
                room_owner_key
            );
            println!("Contract key: {}", contract_key.id());

            match api.get_room(&owner_vk, false).await {
                Ok(room_state) => {
                    match format {
                        OutputFormat::Human => {
                            println!("✓ Successfully retrieved room state");
                            println!(
                                "Configuration version: {}",
                                room_state.configuration.configuration.configuration_version
                            );
                            println!("Room name: {}", room_state.configuration.configuration.name);
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
            println!("DEBUG: Testing WebSocket connection...");

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
    }
}
