use crate::api::ApiClient;
use crate::output::OutputFormat;
use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;

#[derive(Subcommand)]
pub enum RoomCommands {
    /// Create a new room
    Create {
        /// Room name
        #[arg(short, long)]
        name: String,

        /// Your nickname in the room
        #[arg(short = 'N', long)]
        nickname: Option<String>,
    },
    /// List all rooms
    List,
    /// Join a room
    Join {
        /// Room ID
        room_id: String,
    },
    /// Leave a room
    Leave {
        /// Room ID
        room_id: String,
    },
    /// Republish a room to the network
    ///
    /// Re-PUTs the room contract with its current state, making this node
    /// seed it again. Use when the room exists locally but isn't being
    /// served on the network.
    Republish {
        /// Room owner key (base58)
        room_id: String,
    },
    /// Update room configuration (owner only)
    Config {
        /// Room owner key (base58)
        room_id: String,

        /// Set maximum number of user bans remembered
        #[arg(long)]
        max_bans: Option<usize>,

        /// Set maximum number of recent messages stored
        #[arg(long)]
        max_messages: Option<usize>,

        /// Set maximum number of members
        #[arg(long)]
        max_members: Option<usize>,
    },
}

pub async fn execute(command: RoomCommands, api: ApiClient, format: OutputFormat) -> Result<()> {
    match command {
        RoomCommands::Create { name, nickname } => {
            // Ask for nickname if not provided
            let nickname = match nickname {
                Some(n) => n,
                None => {
                    if atty::is(atty::Stream::Stdin) {
                        dialoguer::Input::<String>::new()
                            .with_prompt("Enter your nickname")
                            .default("Anonymous".to_string())
                            .interact_text()?
                    } else {
                        "Anonymous".to_string()
                    }
                }
            };

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Creating room '{}' with nickname '{}'...", name, nickname);
            }

            match api.create_room(name.clone(), nickname).await {
                Ok((owner_key, contract_key)) => {
                    let result = CreateRoomResult {
                        room_name: name,
                        owner_key: bs58::encode(owner_key.as_bytes()).into_string(),
                        contract_key: contract_key.id().to_string(),
                    };

                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Room created successfully!".green());
                            println!("Owner key: {}", result.owner_key);
                            println!("Contract key: {}", result.contract_key);
                            println!("\nTo invite others, use:");
                            println!("  riverctl invite create {}", result.owner_key);
                        }
                        OutputFormat::Json => {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
        RoomCommands::List => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Listing rooms...");
            }

            match api.list_rooms().await {
                Ok(rooms) => {
                    if rooms.is_empty() {
                        match format {
                            OutputFormat::Human => {
                                println!("No rooms found. Use 'riverctl room create' to create a new room.");
                            }
                            OutputFormat::Json => {
                                println!("[]");
                            }
                        }
                    } else {
                        match format {
                            OutputFormat::Human => {
                                println!("\n{} room(s) found:\n", rooms.len());
                                for (owner_key, name, contract_key) in rooms {
                                    println!("Room: {}", name.green());
                                    println!("  Owner key: {}", owner_key);
                                    println!("  Contract key: {}", contract_key);
                                    println!();
                                }
                            }
                            OutputFormat::Json => {
                                let json_rooms: Vec<_> = rooms
                                    .into_iter()
                                    .map(|(owner_key, name, contract_key)| {
                                        serde_json::json!({
                                            "name": name,
                                            "owner_key": owner_key,
                                            "contract_key": contract_key,
                                        })
                                    })
                                    .collect();
                                println!("{}", serde_json::to_string_pretty(&json_rooms)?);
                            }
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
        RoomCommands::Join { room_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Joining room: {}", room_id);
                eprintln!("To join a room, you need an invitation. Use 'riverctl invite accept <invitation-code>'");
            }
            Ok(())
        }
        RoomCommands::Leave { room_id } => {
            if !matches!(format, OutputFormat::Json) {
                eprintln!("Leaving room: {}", room_id);
            }
            // TODO: Implement room leaving
            Ok(())
        }
        RoomCommands::Config {
            room_id,
            max_bans,
            max_messages,
            max_members,
        } => {
            if max_bans.is_none() && max_messages.is_none() && max_members.is_none() {
                // No changes requested, just show current config
                let owner_bytes = bs58::decode(&room_id)
                    .into_vec()
                    .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;
                let owner_key = ed25519_dalek::VerifyingKey::from_bytes(
                    owner_bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid room ID length"))?,
                )
                .map_err(|e| anyhow::anyhow!("Invalid room owner key: {}", e))?;

                let room_state = api.get_room(&owner_key, false).await?;
                let cfg = &room_state.configuration.configuration;
                println!("Current configuration:");
                println!("  max_user_bans: {}", cfg.max_user_bans);
                println!("  max_recent_messages: {}", cfg.max_recent_messages);
                println!("  max_members: {}", cfg.max_members);
                return Ok(());
            }

            let owner_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;
            let owner_key = ed25519_dalek::VerifyingKey::from_bytes(
                owner_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid room ID length"))?,
            )
            .map_err(|e| anyhow::anyhow!("Invalid room owner key: {}", e))?;

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Updating room configuration...");
            }

            match api
                .update_config(&owner_key, |cfg| {
                    if let Some(v) = max_bans {
                        cfg.max_user_bans = v;
                    }
                    if let Some(v) = max_messages {
                        cfg.max_recent_messages = v;
                    }
                    if let Some(v) = max_members {
                        cfg.max_members = v;
                    }
                })
                .await
            {
                Ok(()) => {
                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Configuration updated successfully!".green());
                            if let Some(v) = max_bans {
                                println!("  max_user_bans: {}", v);
                            }
                            if let Some(v) = max_messages {
                                println!("  max_recent_messages: {}", v);
                            }
                            if let Some(v) = max_members {
                                println!("  max_members: {}", v);
                            }
                        }
                        OutputFormat::Json => {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "status": "success",
                                    "room_id": room_id,
                                })
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
        RoomCommands::Republish { room_id } => {
            // Parse the room owner key
            let owner_bytes = bs58::decode(&room_id)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Invalid room ID: {}", e))?;
            let owner_key = ed25519_dalek::VerifyingKey::from_bytes(
                owner_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid room ID length"))?,
            )
            .map_err(|e| anyhow::anyhow!("Invalid room owner key: {}", e))?;

            if !matches!(format, OutputFormat::Json) {
                eprintln!("Republishing room: {}", room_id);
            }

            match api.republish_room(&owner_key).await {
                Ok(()) => {
                    match format {
                        OutputFormat::Human => {
                            println!("{}", "Room republished successfully!".green());
                            println!("The room contract is now being seeded on the network.");
                        }
                        OutputFormat::Json => {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "status": "success",
                                    "room_id": room_id,
                                })
                            );
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    Err(e)
                }
            }
        }
    }
}

#[derive(serde::Serialize)]
struct CreateRoomResult {
    room_name: String,
    owner_key: String,
    contract_key: String,
}
