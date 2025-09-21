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

            println!("Creating room '{}' with nickname '{}'...", name, nickname);

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
            println!("Listing rooms...");

            match api.list_rooms().await {
                Ok(rooms) => {
                    if rooms.is_empty() {
                        println!(
                            "No rooms found. Use 'riverctl room create' to create a new room."
                        );
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
            println!("Joining room: {}", room_id);
            // TODO: Implement room joining via invitation
            println!("To join a room, you need an invitation. Use 'riverctl invite accept <invitation-code>'");
            Ok(())
        }
        RoomCommands::Leave { room_id } => {
            println!("Leaving room: {}", room_id);
            // TODO: Implement room leaving
            Ok(())
        }
    }
}

#[derive(serde::Serialize)]
struct CreateRoomResult {
    room_name: String,
    owner_key: String,
    contract_key: String,
}
