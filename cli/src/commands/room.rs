use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use crate::api::ApiClient;
use crate::output::OutputFormat;

#[derive(Subcommand)]
pub enum RoomCommands {
    /// Create a new room
    Create {
        /// Room name
        #[arg(short, long)]
        name: String,
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
        RoomCommands::Create { name } => {
            // Ask for nickname if not provided
            let nickname = dialoguer::Input::<String>::new()
                .with_prompt("Enter your nickname")
                .default("Anonymous".to_string())
                .interact_text()?;
            
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
                            println!("  river invite create {}", result.contract_key);
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
            // TODO: Implement room listing - this would need to track rooms locally
            println!("No rooms found. Use 'river room create' to create a new room.");
            Ok(())
        }
        RoomCommands::Join { room_id } => {
            println!("Joining room: {}", room_id);
            // TODO: Implement room joining via invitation
            println!("To join a room, you need an invitation. Use 'river invite accept <invitation-code>'");
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